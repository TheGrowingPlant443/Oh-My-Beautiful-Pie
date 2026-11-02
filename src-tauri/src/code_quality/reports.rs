//! Untrusted analyzer reports are data, never filesystem authority or executable markup.
use super::types::{QualityFinding, ReportPreview, Result, Severity};
use serde_json::Map;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::iter::Peekable;
use std::str::Chars;

const MAX_REPORT: usize = 64 * 1024 * 1024;
const MAX_FIELD: usize = 256 * 1024;
const MAX_RESULTS: usize = 100_000;

pub struct ParsedReport {
    pub findings: Vec<QualityFinding>,
    pub warnings: Vec<String>,
    pub result_count: usize,
}

pub struct LinterStatus {
    pub name: String,
    pub status: String,
    pub found_errors: u64,
    pub return_code: Option<i64>,
    pub file_count: Option<u64>,
    pub error: Option<String>,
}

fn parse_json(text: &str) -> Result<Value> {
    if text.len() > MAX_REPORT {
        return Err("Analyzer report exceeds the 64 MiB limit.".into());
    }
    let value: Value = serde_json::from_str(text).map_err(|e| {
        format!(
            "Invalid analyzer JSON at line {}, column {}: {:?}",
            e.line(),
            e.column(),
            e.classify()
        )
    })?;
    fn check(value: &Value, count: &mut usize) -> Result<()> {
        *count += 1;
        if *count > 2_000_000 {
            return Err("Analyzer report contains too many JSON fields.".into());
        }
        match value {
            Value::String(s) if s.len() > MAX_FIELD => {
                return Err("Analyzer report contains a field larger than 256 KiB.".into());
            }
            Value::Array(a) => {
                for v in a {
                    check(v, count)?;
                }
            }
            Value::Object(o) => {
                for (k, v) in o {
                    if k.len() > 1024 {
                        return Err("Analyzer report contains an oversized field name.".into());
                    }
                    check(v, count)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    check(&value, &mut 0)?;
    Ok(value)
}

fn entity_char(entity: &str) -> Option<char> {
    match entity {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" => Some(' '),
        _ => entity
            .strip_prefix("#x")
            .or_else(|| entity.strip_prefix("#X"))
            .and_then(|s| u32::from_str_radix(s, 16).ok())
            .and_then(char::from_u32)
            .or_else(|| {
                entity
                    .strip_prefix('#')
                    .and_then(|s| s.parse::<u32>().ok())
                    .and_then(char::from_u32)
            }),
    }
}

fn decode_entities(text: &str) -> String {
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        decoded.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(end) = rest.find(';').filter(|end| *end <= 12) {
            if let Some(ch) = entity_char(&rest[1..end]) {
                decoded.push(ch);
                rest = &rest[end + 1..];
                continue;
            }
        }
        decoded.push('&');
        rest = &rest[1..];
    }
    decoded.push_str(rest);
    decoded
}

/// Consumes through the closing `>`; the flag is false when the tag never closes.
fn read_tag(chars: &mut Peekable<Chars<'_>>) -> (String, bool) {
    let mut tag = String::new();
    for next in chars.by_ref() {
        if next == '>' {
            return (tag, true);
        }
        tag.push(next);
    }
    (tag, false)
}

fn strip_tags(decoded: &str) -> String {
    let mut out = String::with_capacity(decoded.len());
    let mut chars = decoded.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '<'
            && chars
                .peek()
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == '/' || *c == '!')
        {
            let (tag, closed) = read_tag(&mut chars);
            if closed {
                out.push(' ');
            } else {
                out.push('<');
                out.push_str(&tag);
            }
        } else if !ch.is_control() || matches!(ch, '\n' | '\t') {
            out.push(ch);
        }
    }
    out
}

fn plain(text: &str) -> String {
    // Decode before removing tags, including numeric HTML entities. Never render markup.
    strip_tags(&decode_entities(text)).trim().to_string()
}

fn help_url(value: Option<&str>) -> Option<String> {
    let value = value?;
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let url = reqwest::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url.to_string())
}

#[must_use]
pub fn sarif_severity(level: Option<&str>, security_severity: Option<f64>) -> Severity {
    if let Some(score) = security_severity.filter(|s| s.is_finite() && (0.0..=10.0).contains(s)) {
        return if score >= 9.0 {
            Severity::Critical
        } else if score >= 7.0 {
            Severity::High
        } else if score >= 4.0 {
            Severity::Medium
        } else {
            Severity::Low
        };
    }
    match level.unwrap_or("warning").to_ascii_lowercase().as_str() {
        "error" => Severity::High,
        "note" | "none" => Severity::Info,
        _ => Severity::Medium,
    }
}

#[must_use]
pub fn grep_severity(level: &str) -> Severity {
    match level.to_ascii_uppercase().as_str() {
        "ERROR" => Severity::High,
        "WARNING" => Severity::Medium,
        _ => Severity::Info,
    }
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let a = (*bytes.get(i + 1)? as char).to_digit(16)?;
            let b = (*bytes.get(i + 2)? as char).to_digit(16)?;
            decoded.push((a * 16 + b) as u8);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn decoded_path(raw: &str) -> Option<String> {
    let decoded = percent_decode(raw)?.replace('\\', "/");
    if decoded.is_empty()
        || decoded.contains('%')
        || decoded.chars().any(char::is_control)
        || decoded.contains('?')
        || decoded.contains('#')
    {
        return None;
    }
    let decoded = if let Some(file) = decoded.strip_prefix("file://") {
        // An authority (even localhost) is not a workspace prefix.
        if !file.starts_with('/') {
            return None;
        }
        file.to_string()
    } else {
        decoded
    };
    if decoded.starts_with("//") || decoded.contains(':') || decoded.split('/').any(|s| s == "..") {
        return None;
    }
    Some(decoded)
}

fn workspace_suffix<'a>(candidate: &'a str, bases: &[&str]) -> Option<&'a str> {
    bases
        .iter()
        .filter_map(|base| {
            let base = decoded_path(base)?;
            let base = base.trim_end_matches('/');
            if !base.starts_with('/') || base.is_empty() {
                return None;
            }
            candidate.strip_prefix(base)?.strip_prefix('/')
        })
        .min_by_key(|suffix| suffix.len())
}

#[must_use]
pub fn safe_relative_path(raw: &str, bases: &[&str]) -> Option<String> {
    let decoded = decoded_path(raw)?;
    let mut candidate = decoded.as_str();
    if candidate.starts_with('/') {
        // Only explicitly known container workspaces are authoritative.
        candidate = workspace_suffix(candidate, bases)?;
    }
    let parts: Vec<_> = candidate
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if parts.is_empty() || parts.iter().any(|s| s.contains(':') || *s == "..") {
        return None;
    }
    Some(parts.join("/"))
}

fn positive(value: Option<&Value>) -> Option<u32> {
    value
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
}

fn numeric(value: Option<&Value>) -> Option<f64> {
    value.and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

fn category(metadata: &Value) -> String {
    metadata
        .get("category")
        .and_then(Value::as_str)
        .map(plain)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Code quality".into())
}

fn is_secret(tool: &str, rule: &str, metadata: &Value) -> bool {
    let mut context = format!("{tool} {rule}").to_ascii_lowercase();
    for key in ["category", "tags", "technology", "cwe"] {
        if let Some(value) = metadata.get(key) {
            context.push_str(&value.to_string().to_ascii_lowercase());
        }
    }
    [
        "secret",
        "credential",
        "betterleaks",
        "gitleaks",
        "private-key",
        "private_key",
        "password",
        "api-key",
        "api_key",
        "access-key",
        "access_key",
        "auth-token",
    ]
    .iter()
    .any(|s| context.contains(s))
}

fn finding_message(tool: &str, rule: &str, metadata: &Value, message: &str) -> String {
    if is_secret(tool, rule, metadata) {
        "Potential secret detected; review the rule and source location (matched value redacted)."
            .into()
    } else {
        plain(message)
    }
}

fn warning(warnings: &mut Vec<String>, text: String) {
    // Keep adversarial reports from multiplying diagnostics without bound.
    if warnings.len() < 1000 {
        warnings.push(text);
    } else if warnings.len() == 1000 {
        warnings.push("Further analyzer warnings omitted.".into());
    }
}

fn resolve_base(run: &Value, id: &str, seen: &mut BTreeSet<String>) -> Option<String> {
    if seen.len() >= 32 || !seen.insert(id.to_string()) {
        return None;
    }
    let base = run.get("originalUriBaseIds")?.get(id)?;
    let uri = base.get("uri").and_then(Value::as_str).unwrap_or("");
    if let Some(parent) = base.get("uriBaseId").and_then(Value::as_str) {
        let parent = resolve_base(run, parent, seen)?;
        Some(format!("{}/{}", parent.trim_end_matches('/'), uri))
    } else {
        Some(uri.to_string())
    }
}

fn artifact_uri(run: &Value, location: &Value, depth: usize) -> Option<String> {
    if depth > 8 {
        return None;
    }
    if location.get("uri").is_none() {
        let index = usize::try_from(location.get("index")?.as_u64()?).ok()?;
        let artifact = run
            .get("artifacts")?
            .as_array()?
            .get(index)?
            .get("location")?;
        return artifact_uri(run, artifact, depth + 1);
    }
    let uri = location.get("uri")?.as_str()?;
    if let Some(id) = location.get("uriBaseId").and_then(Value::as_str) {
        let base = resolve_base(run, id, &mut BTreeSet::new())?;
        // An absolute child cannot override its declared base.
        if uri.starts_with('/') || uri.starts_with('\\') || uri.contains(':') {
            return None;
        }
        Some(format!("{}/{}", base.trim_end_matches('/'), uri))
    } else {
        Some(uri.to_string())
    }
}

fn component<'a>(run: &'a Value, reference: Option<&Value>) -> Option<&'a Value> {
    if let Some(reference) = reference {
        let extensions = run.pointer("/tool/extensions")?.as_array()?;
        if let Some(index) = reference.get("index").and_then(Value::as_u64) {
            return extensions.get(usize::try_from(index).ok()?);
        }
        extensions.iter().find(|c| {
            reference
                .get("guid")
                .and_then(Value::as_str)
                .is_some_and(|g| c.get("guid").and_then(Value::as_str) == Some(g))
                || reference
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| c.get("name").and_then(Value::as_str) == Some(n))
        })
    } else {
        run.pointer("/tool/driver")
    }
}

fn sarif_rule<'a>(run: &'a Value, result: &Value) -> Option<&'a Value> {
    let reference = result.get("rule");
    let explicit_component = reference.and_then(|r| r.get("toolComponent"));
    let target = component(run, explicit_component);
    let rules = target
        .and_then(|t| t.get("rules"))
        .and_then(Value::as_array);
    let id = result
        .get("ruleId")
        .and_then(Value::as_str)
        .or_else(|| reference.and_then(|r| r.get("id")).and_then(Value::as_str));
    if let Some(id) = id {
        if let Some(rule) = rules.and_then(|rules| {
            rules
                .iter()
                .find(|r| r.get("id").and_then(Value::as_str) == Some(id))
        }) {
            return Some(rule);
        }
        if explicit_component.is_none() {
            // Some producers omit toolComponent on an otherwise unique extension id.
            let mut matches = run
                .pointer("/tool/extensions")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(|extension| extension.get("rules").and_then(Value::as_array))
                .flatten()
                .filter(|rule| rule.get("id").and_then(Value::as_str) == Some(id));
            let first = matches.next()?;
            if matches.next().is_none() {
                return Some(first);
            }
        }
        // A stale index must never attach another rule's help/severity to this id.
        return None;
    }
    let index = result
        .get("ruleIndex")
        .or_else(|| reference.and_then(|r| r.get("index")))?
        .as_u64()?;
    rules?.get(usize::try_from(index).ok()?)
}

fn sarif_message(run: &Value, result: &Value, rule: &Value) -> Option<String> {
    let message = result.get("message")?;
    if let Some(text) = message
        .get("text")
        .or_else(|| message.get("markdown"))
        .and_then(Value::as_str)
    {
        return Some(text.to_string());
    }
    let id = message.get("id")?.as_str()?;
    let template = rule
        .get("messageStrings")
        .and_then(|m| m.get(id))
        .or_else(|| {
            run.pointer("/tool/driver/globalMessageStrings")
                .and_then(|m| m.get(id))
        })?;
    let mut text = template
        .get("text")
        .or_else(|| template.get("markdown"))?
        .as_str()?
        .to_string();
    if let Some(args) = message.get("arguments").and_then(Value::as_array) {
        for (i, arg) in args.iter().enumerate() {
            if let Some(arg) = arg.as_str() {
                text = text.replace(&format!("{{{i}}}"), arg);
            }
        }
    }
    Some(text)
}

fn sarif_notification_warnings(tool: &str, invocation: &Value, warnings: &mut Vec<String>) {
    for field in [
        "toolExecutionNotifications",
        "toolConfigurationNotifications",
    ] {
        let notifications = invocation
            .get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for notification in notifications {
            let level = notification
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or("warning");
            if level != "note" {
                warning(
                    warnings,
                    format!(
                        "{tool}: SARIF {field} ({level}); inspect the redacted diagnostic report."
                    ),
                );
            }
        }
    }
}

fn sarif_invocation_warnings(
    tool: &str,
    run: &Value,
    run_index: usize,
    warnings: &mut Vec<String>,
) {
    let invocations = run
        .get("invocations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for invocation in invocations {
        if invocation
            .get("executionSuccessful")
            .and_then(Value::as_bool)
            == Some(false)
        {
            warning(
                warnings,
                format!(
                    "{tool}: SARIF run {} reports unsuccessful execution.",
                    run_index + 1
                ),
            );
        }
        sarif_notification_warnings(tool, invocation, warnings);
    }
}

fn collect_sarif_results(
    tool: &str,
    run: &Value,
    run_index: usize,
    results: &[Value],
    path_bases: &[&str],
    parsed: &mut ParsedReport,
) -> Result<()> {
    for (index, result) in results.iter().enumerate() {
        if !result.is_object() {
            return Err("SARIF contains a non-object result.".into());
        }
        let rule = sarif_rule(run, result).unwrap_or(&Value::Null);
        let rule_id = result
            .get("ruleId")
            .and_then(Value::as_str)
            .or_else(|| result.pointer("/rule/id").and_then(Value::as_str))
            .or_else(|| rule.get("id").and_then(Value::as_str));
        let Some(rule_id) = rule_id.filter(|s| !s.trim().is_empty()) else {
            warning(&mut parsed.warnings, format!("{tool}: SARIF run {} result {} has no resolvable rule id; finding retained with an unknown rule.", run_index + 1, index + 1));
            parsed.findings.push(sarif_finding(
                tool,
                run,
                result,
                rule,
                "unknown-rule",
                path_bases,
                &mut parsed.warnings,
            ));
            continue;
        };
        parsed.findings.push(sarif_finding(
            tool,
            run,
            result,
            rule,
            rule_id,
            path_bases,
            &mut parsed.warnings,
        ));
    }
    Ok(())
}

pub fn parse_sarif(tool: &str, text: &str, path_bases: &[&str]) -> Result<ParsedReport> {
    let root = parse_json(text)?;
    if root.get("version").and_then(Value::as_str) != Some("2.1.0") {
        return Err("Expected a SARIF 2.1.0 report.".into());
    }
    let runs = root
        .get("runs")
        .and_then(Value::as_array)
        .ok_or("SARIF report has no runs array.")?;
    let mut parsed = ParsedReport {
        findings: Vec::new(),
        warnings: Vec::new(),
        result_count: 0,
    };
    if runs.is_empty() {
        warning(
            &mut parsed.warnings,
            "SARIF contains no runs; coverage is unavailable.".into(),
        );
    }
    for (run_index, run) in runs.iter().enumerate() {
        sarif_invocation_warnings(tool, run, run_index, &mut parsed.warnings);
        let results = match run.get("results") {
            Some(Value::Array(results)) => results,
            None => {
                warning(
                    &mut parsed.warnings,
                    format!(
                        "{tool}: SARIF run {} has no results; coverage is unavailable.",
                        run_index + 1
                    ),
                );
                continue;
            }
            _ => return Err("SARIF results must be an array.".into()),
        };
        parsed.result_count = parsed
            .result_count
            .checked_add(results.len())
            .ok_or("SARIF result count overflow.")?;
        if parsed.result_count > MAX_RESULTS {
            return Err("SARIF exceeds the 100,000 result limit; narrow the scan.".into());
        }
        collect_sarif_results(tool, run, run_index, results, path_bases, &mut parsed)?;
    }
    parsed.findings = merge_findings(parsed.findings);
    Ok(parsed)
}

fn sarif_finding(
    tool: &str,
    run: &Value,
    result: &Value,
    rule: &Value,
    rule_id: &str,
    bases: &[&str],
    warnings: &mut Vec<String>,
) -> QualityFinding {
    let properties = rule.get("properties").unwrap_or(&Value::Null);
    let score = numeric(result.pointer("/properties/security-severity"))
        .or_else(|| numeric(properties.get("security-severity")))
        .or_else(|| numeric(result.pointer("/properties/cvss")))
        .or_else(|| numeric(properties.get("cvss")))
        .or_else(|| numeric(properties.get("cvssScore")));
    let level = result.get("level").and_then(Value::as_str).or_else(|| {
        rule.pointer("/defaultConfiguration/level")
            .and_then(Value::as_str)
    });
    let message = sarif_message(run, result, rule).unwrap_or_else(|| {
        warning(
            warnings,
            format!(
                "{tool}: result for {} has no resolvable message.",
                plain(rule_id)
            ),
        );
        "Analyzer supplied no message; inspect the report.".into()
    });
    let locations = result.get("locations").and_then(Value::as_array);
    let mut path = None;
    let mut line = None;
    let mut column = None;
    let mut end_line = None;
    if let Some(locations) = locations {
        for location in locations {
            let Some(physical) = location.get("physicalLocation") else {
                continue;
            };
            if let Some(artifact) = physical.get("artifactLocation") {
                path =
                    artifact_uri(run, artifact, 0).and_then(|uri| safe_relative_path(&uri, bases));
                if path.is_none() {
                    warning(warnings, format!("{tool}: an unsafe or unknown-base artifact path for rule {} is display-only; no source path was assigned.", plain(rule_id)));
                    continue;
                }
                line = positive(physical.pointer("/region/startLine"));
                column = positive(physical.pointer("/region/startColumn"));
                end_line = positive(physical.pointer("/region/endLine"));
                break;
            }
        }
    }
    QualityFinding {
        id: String::new(),
        tools: vec![plain(tool)],
        rule_id: plain(rule_id),
        category: category(properties),
        severity: sarif_severity(level, score),
        message: finding_message(tool, rule_id, properties, &message),
        path,
        line,
        column,
        end_line,
        help_url: help_url(rule.get("helpUri").and_then(Value::as_str)),
        hotspot: false,
    }
}

/// Warns for every tool error and returns how many were fatal.
fn grep_error_warnings(tool: &str, root: &Value, warnings: &mut Vec<String>) -> Result<usize> {
    let Some(errors) = root.get("errors") else {
        return Ok(0);
    };
    let errors = errors
        .as_array()
        .ok_or("Grep report errors must be an array.")?;
    let mut fatal_errors = 0;
    for error in errors {
        let level = error
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("error");
        if !matches!(
            level.to_ascii_lowercase().as_str(),
            "warn" | "warning" | "info"
        ) {
            fatal_errors += 1;
        }
        // Tool errors can contain arbitrary matched source in message/spans: only expose type.
        let kind = error
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("analysis error");
        warning(
            warnings,
            format!(
                "{tool}: {} ({}) reported; coverage is incomplete. Inspect redacted diagnostics.",
                plain(kind),
                plain(level)
            ),
        );
    }
    Ok(fatal_errors)
}

fn grep_finding(
    tool: &str,
    result: &Value,
    path_bases: &[&str],
    warnings: &mut Vec<String>,
) -> Result<QualityFinding> {
    let check_id = result
        .get("check_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("Grep result is missing check_id.")?;
    let extra = result
        .get("extra")
        .ok_or("Grep result is missing extra metadata.")?;
    let metadata = extra.get("metadata").unwrap_or(&Value::Null);
    let rule_id = metadata
        .get("original_rule_id")
        .or_else(|| metadata.get("original-rule-id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(check_id);
    let message = extra
        .get("message")
        .and_then(Value::as_str)
        .ok_or("Grep result is missing its message.")?;
    let path = result
        .get("path")
        .and_then(Value::as_str)
        .and_then(|p| safe_relative_path(p, path_bases));
    if path.is_none() {
        warning(warnings, format!("{tool}: an absent, unsafe or unknown-base path for rule {} is display-only; no source path was assigned.", plain(rule_id)));
    }
    let (line, column, end_line) = if path.is_some() {
        (
            positive(result.pointer("/start/line")),
            positive(result.pointer("/start/col")),
            positive(result.pointer("/end/line")),
        )
    } else {
        (None, None, None)
    };
    let help = [
        metadata.get("source"),
        metadata.get("help_url"),
        metadata.get("help"),
        metadata
            .get("references")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first()),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find_map(|s| help_url(Some(s)));
    Ok(QualityFinding {
        id: String::new(),
        tools: vec![plain(tool)],
        rule_id: plain(rule_id),
        category: category(metadata),
        severity: grep_severity(
            extra
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or("INFO"),
        ),
        message: finding_message(tool, rule_id, metadata, message),
        path,
        line,
        column,
        end_line,
        help_url: help,
        hotspot: false,
    })
}

pub fn parse_grep_json(tool: &str, text: &str, path_bases: &[&str]) -> Result<ParsedReport> {
    let root = parse_json(text)?;
    let results = root
        .get("results")
        .and_then(Value::as_array)
        .ok_or("Grep report has no results array.")?;
    if results.len() > MAX_RESULTS {
        return Err("Grep report exceeds the 100,000 result limit; narrow the scan.".into());
    }
    let mut parsed = ParsedReport {
        findings: Vec::new(),
        warnings: Vec::new(),
        result_count: results.len(),
    };
    let fatal_errors = grep_error_warnings(tool, &root, &mut parsed.warnings)?;
    if fatal_errors > 0 && results.is_empty() {
        return Err(format!("{tool} reported {fatal_errors} analysis error(s) and no usable findings; this is not a clean scan."));
    }
    for result in results {
        parsed.findings.push(grep_finding(
            tool,
            result,
            path_bases,
            &mut parsed.warnings,
        )?);
    }
    if let Some(skipped) = root
        .pointer("/paths/skipped")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        warning(
            &mut parsed.warnings,
            format!(
                "{tool}: {} paths were skipped; coverage is incomplete.",
                skipped.len()
            ),
        );
    }
    if results.is_empty() {
        warning(&mut parsed.warnings, format!("{tool}: no findings were reported; applicable-rule/file coverage must be checked separately."));
    }
    parsed.findings = merge_findings(parsed.findings);
    Ok(parsed)
}

fn linter_count(linter: &Value, field: &str) -> Result<Option<u64>> {
    match linter.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("MegaLinter {field} is not a nonnegative integer.")),
    }
}

fn failed_linter_message(
    name: &str,
    found_errors: u64,
    warnings: u64,
    return_code: Option<i64>,
) -> String {
    // Do not quote files_lint_results: it may contain unredacted Betterleaks matches.
    let exit = return_code
        .map(|v| v.to_string())
        .unwrap_or_else(|| "unknown".into());
    if found_errors > 0 || warnings > 0 {
        format!("{} reported {found_errors} error(s), {warnings} warning(s); inspect its reports (exit {exit}).", plain(name))
    } else {
        format!(
            "{} failed without a finding count (exit {exit}); coverage is incomplete.",
            plain(name)
        )
    }
}

fn linter_status(linter: &Value) -> Result<LinterStatus> {
    let name = linter
        .get("name")
        .or_else(|| linter.get("linter_name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("MegaLinter entry is missing its linter name.")?;
    let active = linter
        .get("is_active")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let return_code = match linter.get("return_code") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .ok_or("MegaLinter return_code is not an integer.")?,
        ),
    };
    let found_errors = linter_count(linter, "total_number_errors")?
        .or(linter_count(linter, "number_errors")?)
        .unwrap_or(0);
    let warnings = linter_count(linter, "total_number_warnings")?.unwrap_or(0);
    let file_count = linter_count(linter, "files_number")?.or_else(|| {
        linter
            .get("files")
            .and_then(Value::as_array)
            .map(|a| a.len() as u64)
    });
    let raw_status = linter.get("status").and_then(Value::as_str);
    let mut status = if !active {
        "skipped".into()
    } else {
        raw_status
            .map(|s| plain(s).to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".into())
    };
    let failed = active
        && (return_code.is_some_and(|c| c != 0)
            || matches!(status.as_str(), "error" | "failed" | "failure"));
    let error = if failed {
        Some(failed_linter_message(
            name,
            found_errors,
            warnings,
            return_code,
        ))
    } else if active && status == "unknown" {
        Some("Linter status is missing; coverage is unknown.".into())
    } else {
        None
    };
    if active && file_count == Some(0) && !failed {
        status = "skipped".into();
    }
    Ok(LinterStatus {
        name: plain(name),
        status,
        found_errors,
        return_code,
        file_count,
        error,
    })
}

/// Field names follow MegaLinter v10.1.0 JsonReporter.linter_fields, not SARIF coverage.
/// https://github.com/oxsecurity/megalinter/blob/v10.1.0/megalinter/reporters/JsonReporter.py
pub fn parse_megalinter_json(text: &str) -> Result<Vec<LinterStatus>> {
    let root = parse_json(text)?;
    let linters = root
        .get("linters")
        .and_then(Value::as_array)
        .ok_or("MegaLinter JSON has no linters array.")?;
    if linters.len() > 10_000 {
        return Err("MegaLinter JSON contains too many linter entries.".into());
    }
    if linters.is_empty() {
        return Err("MegaLinter reported no linter outcomes; coverage is unavailable.".into());
    }
    let mut statuses = Vec::with_capacity(linters.len());
    for linter in linters {
        statuses.push(linter_status(linter)?);
    }
    Ok(statuses)
}

fn normalized(text: &str) -> String {
    plain(text).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn finding_key(finding: &QualityFinding) -> String {
    // A structured key prevents delimiter collisions in attacker-controlled messages/paths.
    serde_json::to_string(&(
        finding.rule_id.trim(),
        &finding.path,
        finding.line,
        finding.column,
        finding.end_line,
        normalized(&finding.message),
    ))
    .expect("finding keys contain only JSON-serializable scalar fields")
}

#[must_use]
pub fn merge_findings(findings: Vec<QualityFinding>) -> Vec<QualityFinding> {
    let mut merged: BTreeMap<String, QualityFinding> = BTreeMap::new();
    for mut finding in findings {
        finding.rule_id = plain(&finding.rule_id).trim().to_string();
        finding.message = plain(&finding.message);
        finding.path = finding
            .path
            .as_deref()
            .and_then(|p| safe_relative_path(p, &[]));
        if finding.path.is_none() {
            finding.line = None;
            finding.column = None;
            finding.end_line = None;
        }
        finding.tools.sort();
        finding.tools.dedup();
        finding.help_url = help_url(finding.help_url.as_deref());
        let key = finding_key(&finding);
        if let Some(existing) = merged.get_mut(&key) {
            existing.tools.extend(finding.tools);
            existing.tools.sort();
            existing.tools.dedup();
            existing.severity = existing.severity.min(finding.severity);
            existing.hotspot |= finding.hotspot;
            if existing.help_url.is_none() {
                existing.help_url = finding.help_url;
            }
        } else {
            finding.id = format!("{:x}", Sha256::digest(key.as_bytes()))[..16].to_string();
            merged.insert(key, finding);
        }
    }
    let mut findings: Vec<_> = merged.into_values().collect();
    findings.sort_by(|a, b| {
        (&a.severity, &a.path, a.line, a.column, &a.rule_id, &a.id).cmp(&(
            &b.severity,
            &b.path,
            b.line,
            b.column,
            &b.rule_id,
            &b.id,
        ))
    });
    findings
}

fn base64(bytes: &[u8], url_safe: bool, padding: bool) -> String {
    let table = if url_safe {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    } else {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    };
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(table[((n >> 18) & 63) as usize] as char);
        out.push(table[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(table[((n >> 6) & 63) as usize] as char);
        } else if padding {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(table[(n & 63) as usize] as char);
        } else if padding {
            out.push('=');
        }
    }
    out
}

fn add_base64_forms(forms: &mut BTreeSet<String>, secret: &str) {
    for url_safe in [false, true] {
        for padding in [false, true] {
            let encoded = base64(secret.as_bytes(), url_safe, padding);
            if encoded.len() >= 6 {
                forms.insert(encoded);
            }
        }
    }
}

/// Returns the unreserved-only, form (`+` for space) and every-byte percent encodings.
fn percent_encodings(secret: &str) -> [String; 3] {
    use std::fmt::Write as _;
    let mut encoded = String::new();
    let mut form = String::new();
    let mut all_encoded = String::new();
    for &b in secret.as_bytes() {
        let _ = write!(all_encoded, "%{b:02X}");
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            encoded.push(b as char);
            form.push(b as char);
        } else {
            let _ = write!(encoded, "%{b:02X}");
            if b == b' ' {
                form.push('+');
            } else {
                let _ = write!(form, "%{b:02X}");
            }
        }
    }
    [encoded, form, all_encoded]
}

/// Lowercase hex escapes, not the secret's literal characters.
fn lowercase_escapes(value: &str) -> Option<String> {
    let mut lowercase = value.as_bytes().to_vec();
    let mut i = 0;
    while i + 2 < lowercase.len() {
        if lowercase[i] == b'%' {
            lowercase[i + 1].make_ascii_lowercase();
            lowercase[i + 2].make_ascii_lowercase();
            i += 3;
        } else {
            i += 1;
        }
    }
    String::from_utf8(lowercase).ok()
}

fn add_percent_forms(forms: &mut BTreeSet<String>, secret: &str) {
    for value in percent_encodings(secret) {
        if value.len() < 6 {
            continue;
        }
        forms.extend(lowercase_escapes(&value));
        forms.insert(value);
    }
}

#[must_use]
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut forms = BTreeSet::new();
    let secrets = secrets.iter().copied().filter(|s| !s.is_empty());
    for secret in secrets {
        forms.insert(secret.to_string());
        add_base64_forms(&mut forms, secret);
        add_percent_forms(&mut forms, secret);
    }
    let mut forms: Vec<_> = forms.into_iter().collect();
    forms.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = text.to_string();
    for form in forms {
        out = out.replace(&form, "[REDACTED]");
    }
    out
}

#[must_use]
pub fn preview_text(text: &str, max_bytes: usize) -> ReportPreview {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    ReportPreview {
        text: text[..end].to_string(),
        truncated: end < text.len(),
    }
}

fn sensitive_field(key: &str) -> bool {
    let key: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        key.as_str(),
        "secret"
            | "secrets"
            | "match"
            | "matched"
            | "matchedvalue"
            | "matchedvalues"
            | "matchedtext"
            | "snippet"
            | "snippets"
            | "lines"
            | "sourceline"
            | "sourcelines"
            | "metavars"
            | "metavariables"
            | "abstractcontent"
            | "propagatedvalue"
            | "contents"
            | "fingerprint"
    )
}

fn mask_json_strings(value: &mut Value, secrets: &[&str]) {
    match value {
        Value::String(text) => *text = redact(text, secrets),
        Value::Array(values) => {
            for value in values {
                mask_json_strings(value, secrets);
            }
        }
        Value::Object(object) => {
            for value in object.values_mut() {
                mask_json_strings(value, secrets);
            }
        }
        _ => {}
    }
}

fn mask_message(message: &mut Value) {
    let text = Value::String("Potential secret detected (matched value redacted).".into());
    if let Value::Object(object) = message {
        object.clear();
        object.insert("text".into(), text);
    } else {
        *message = text;
    }
}

fn collect_match_values(value: &Value, matched: bool, values: &mut BTreeSet<String>) {
    match value {
        Value::String(text) if matched && !text.is_empty() => {
            values.insert(text.clone());
        }
        Value::Array(array) => {
            for value in array {
                collect_match_values(value, matched, values);
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                collect_match_values(value, matched || sensitive_field(key), values);
            }
        }
        _ => {}
    }
}

fn secret_result_flags(json: &Value, tool: &str, secret_context: bool) -> Vec<bool> {
    if json.get("tool").is_none() {
        return Vec::new();
    }
    json.get("results")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .map(|result| {
                    let rule = sarif_rule(json, result).unwrap_or(&Value::Null);
                    let id = result
                        .get("ruleId")
                        .or_else(|| rule.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    secret_context
                        || is_secret(tool, id, rule.get("properties").unwrap_or(&Value::Null))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn mask_sensitive_values(object: &mut Map<String, Value>, values: &mut BTreeSet<String>) {
    let sensitive_values = object.iter().filter(|(key, _)| sensitive_field(key));
    for (_, value) in sensitive_values {
        collect_match_values(value, /* matched= */ true, values);
    }
    if values.is_empty() {
        return;
    }
    let secrets: Vec<&str> = values.iter().map(String::as_str).collect();
    for value in object.values_mut() {
        mask_json_strings(value, &secrets);
    }
}

fn mask_secret_messages(object: &mut Map<String, Value>) {
    if let Some(message) = object.get_mut("message") {
        mask_message(message);
    }
    if let Some(Value::Object(extra)) = object.get_mut("extra") {
        if let Some(message) = extra.get_mut("message") {
            mask_message(message);
        }
    }
}

fn strip_children(object: &mut Map<String, Value>, secret: bool, secret_results: &[bool]) {
    for (key, value) in object.iter_mut() {
        match value {
            Value::Array(results) if key == "results" && !secret_results.is_empty() => {
                for (result, secret) in results.iter_mut().zip(secret_results) {
                    strip_fields_inner(result, *secret);
                }
            }
            _ => strip_fields_inner(value, secret),
        }
    }
}

fn strip_object(
    object: &mut Map<String, Value>,
    values: &mut BTreeSet<String>,
    secret_context: bool,
    secret_results: &[bool],
) {
    mask_sensitive_values(object, values);
    object.retain(|key, _| !sensitive_field(key));
    let metadata = object
        .get("metadata")
        .or_else(|| object.get("extra").and_then(|extra| extra.get("metadata")))
        .unwrap_or(&Value::Null);
    let rule = object
        .get("check_id")
        .or_else(|| object.get("ruleId"))
        .or_else(|| object.get("RuleID"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let secret = secret_context || is_secret("", rule, metadata);
    if secret {
        mask_secret_messages(object);
    }
    strip_children(object, secret, secret_results);
}

fn strip_fields_inner(json: &mut Value, inherited_secret: bool) {
    let tool = json
        .pointer("/tool/driver/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let secret_context = inherited_secret || is_secret(tool, "", &Value::Null);
    // Preserve per-rule secret provenance before removing snippets and rule metadata.
    let secret_results = secret_result_flags(json, tool, secret_context);
    let mut values = BTreeSet::new();
    if json.get("message").is_some() || json.pointer("/extra/message").is_some() {
        // SARIF can put Secret/Match inside properties while repeating it in message.text.
        collect_match_values(json, /* matched= */ false, &mut values);
    }
    match json {
        Value::Array(array) => {
            for value in array {
                strip_fields_inner(value, secret_context);
            }
        }
        Value::Object(object) => strip_object(object, &mut values, secret_context, &secret_results),
        _ => {}
    }
}

pub fn strip_secret_fields(json: &mut Value) {
    strip_fields_inner(json, /* inherited_secret= */ false);
}

#[cfg(test)]
mod secret_context_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sarif_secret_driver_and_nested_matched_values_scrub_messages() {
        let mut report = json!({"version":"2.1.0","runs":[
            {"tool":{"driver":{"name":"Betterleaks","rules":[{"id":"slack-webhook"}]}},
             "results":[{"ruleId":"slack-webhook","message":{"text":"Found NEVER_PRINT"}}]},
            {"tool":{"driver":{"name":"MegaLinter","rules":[{"id":"token-pattern","properties":{"tags":["secrets"]}}]}},
             "results":[{"ruleIndex":0,"message":{"text":"Found NEVER_PRINT"}}]},
            {"tool":{"driver":{"name":"Other"}},"results":[{"ruleId":"opaque",
             "message":{"text":"Found NEVER_PRINT"},"properties":{"Secret":"NEVER_PRINT"}}]}
        ]});
        strip_secret_fields(&mut report);
        assert!(!report.to_string().contains("NEVER_PRINT"));
        let parsed = parse_sarif("MegaLinter", &report.to_string(), &[]).unwrap();
        assert_eq!(parsed.result_count, 3);
        assert!(parsed
            .findings
            .iter()
            .all(|finding| finding.message.contains("redacted")
                || finding.message.contains("REDACTED")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn grep(tool: &str, rule: &str, path: &str, message: &str) -> QualityFinding {
        parse_grep_json(tool, &json!({"results":[{"check_id":rule,"path":path,"start":{"line":2,"col":3},"end":{"line":2},"extra":{"message":message,"severity":"ERROR","metadata":{"category":"security"}}}],"errors":[]}).to_string(), &["/workspace"]).unwrap().findings.remove(0)
    }

    #[test]
    fn sarif_all_runs_indexes_extensions_and_missing_locations() {
        let report = json!({"version":"2.1.0","runs":[{
            "tool":{"driver":{"rules":[{"id":"indexed","helpUri":"https://example.com/rule","properties":{"security-severity":"9.2"}}]},"extensions":[{"name":"plugin","rules":[{"id":"extension","defaultConfiguration":{"level":"note"}}]}]},
            "results":[{"ruleIndex":0,"message":{"text":"<b>Risk</b>"}},{"rule":{"index":0,"toolComponent":{"index":0}},"message":{"text":"Extension"}}]
        },{"tool":{"driver":{"rules":[{"id":"wrong"},{"id":"right","properties":{"security-severity":4.2}}]}},"results":[{"ruleId":"right","ruleIndex":0,"message":{"text":"Second run"}}]}]});
        let parsed = parse_sarif("MegaLinter", &report.to_string(), &[]).unwrap();
        assert_eq!(parsed.result_count, 3);
        assert_eq!(parsed.findings.len(), 3);
        let first = parsed
            .findings
            .iter()
            .find(|f| f.rule_id == "indexed")
            .unwrap();
        assert_eq!(first.severity, Severity::Critical);
        assert_eq!(first.message, "Risk");
        assert_eq!(first.path, None);
        assert_eq!(first.line, None);
        assert_eq!(
            parsed
                .findings
                .iter()
                .find(|f| f.rule_id == "extension")
                .unwrap()
                .severity,
            Severity::Info
        );
        assert_eq!(
            parsed
                .findings
                .iter()
                .find(|f| f.rule_id == "right")
                .unwrap()
                .severity,
            Severity::Medium
        );
    }

    #[test]
    fn sarif_uri_bases_artifacts_and_message_references() {
        let report = json!({"version":"2.1.0","runs":[{
            "tool":{"driver":{"rules":[{"id":"rule","messageStrings":{"m":{"text":"Unsafe {0}"}}}]}},
            "originalUriBaseIds":{"ROOT":{"uri":"file:///workspace/"},"SRC":{"uri":"src/","uriBaseId":"ROOT"}},
            "artifacts":[{"location":{"uri":"space%20%C3%A6.ts","uriBaseId":"SRC"}}],
            "results":[{"ruleId":"rule","message":{"id":"m","arguments":["call"]},"locations":[{"physicalLocation":{"artifactLocation":{"index":0},"region":{"startLine":4,"startColumn":2,"endLine":5}}}]}]
        }]});
        let parsed = parse_sarif("test", &report.to_string(), &["/workspace"]).unwrap();
        let f = &parsed.findings[0];
        assert_eq!(f.path.as_deref(), Some("src/space æ.ts"));
        assert_eq!((f.line, f.column, f.end_line), (Some(4), Some(2), Some(5)));
        assert_eq!(f.message, "Unsafe call");
    }

    #[test]
    fn unsafe_paths_and_unknown_bases_are_never_host_paths() {
        for path in [
            "../../x",
            "C:\\x",
            "\\\\server\\share",
            "file:///etc/passwd",
            "/workspace/../x",
            "/workspace-other/x",
            "%2e%2e/x",
            "/workspace/%252e%252e/x",
            "https://evil/x",
            "file://server/workspace/x",
        ] {
            assert_eq!(
                safe_relative_path(path, &["/workspace", "/tmp/lint"]),
                None,
                "{path}"
            );
        }
        assert_eq!(
            safe_relative_path("/tmp/lint/src/x.py", &["/tmp/lint"]),
            Some("src/x.py".into())
        );
        assert_eq!(
            safe_relative_path("./src/x.py", &[]),
            Some("src/x.py".into())
        );
        let report = json!({"version":"2.1.0","runs":[{"results":[{"ruleId":"x","message":{"text":"x"},"locations":[{"physicalLocation":{"artifactLocation":{"uri":"x","uriBaseId":"UNKNOWN"},"region":{"startLine":9}}}]}]}]});
        let parsed = parse_sarif("test", &report.to_string(), &["/workspace"]).unwrap();
        assert_eq!(parsed.findings[0].path, None);
        assert_eq!(parsed.findings[0].line, None);
        assert!(!parsed.warnings.is_empty());
    }

    #[test]
    fn engines_merge_by_rule_range_message_not_line_alone() {
        let a = grep(
            "Opengrep",
            "omp-quality.eval",
            "/workspace/a.js",
            "Risky  eval",
        );
        let b = grep("Semgrep CE", "omp-quality.eval", "a.js", "Risky\neval");
        let c = grep("Semgrep CE", "different-rule", "a.js", "Risky eval");
        let merged = merge_findings(vec![a.clone(), b.clone(), c]);
        assert_eq!(merged.len(), 2);
        let f = merged
            .iter()
            .find(|f| f.rule_id == "omp-quality.eval")
            .unwrap();
        assert_eq!(f.tools, vec!["Opengrep", "Semgrep CE"]);
        assert_eq!(f.id, merge_findings(vec![b, a])[0].id);
    }

    #[test]
    fn grep_errors_are_not_findings_or_clean_execution() {
        let value = json!({"results":[],"errors":[{"level":"error","type":"ParseError","message":"source secret"}]});
        assert!(parse_grep_json("Semgrep CE", &value.to_string(), &[]).is_err());
        let mut value = value;
        value["results"] = json!([{"check_id":"rule","path":"a.py","extra":{"message":"finding","severity":"WARNING"}}]);
        let parsed = parse_grep_json("Semgrep CE", &value.to_string(), &[]).unwrap();
        assert_eq!(parsed.findings.len(), 1);
        assert_eq!(parsed.findings[0].severity, Severity::Medium);
        assert!(parsed.warnings[0].contains("ParseError"));
        assert!(!parsed.warnings[0].contains("source secret"));
        // A findings-only nonzero exit is represented by results, not a synthetic engine error.
        value["errors"] = json!([]);
        assert!(parse_grep_json("Opengrep", &value.to_string(), &[])
            .unwrap()
            .warnings
            .is_empty());
    }

    #[test]
    fn megalinter_json_only_failure_and_counts_are_retained() {
        let report = json!({"status":"error","linters":[
            {"name":"PYTHON_RUFF","is_active":true,"status":"error","return_code":2,"files_number":3,"number_errors":0,"total_number_errors":0},
            {"name":"REPOSITORY_BETTERLEAKS","status":"error","return_code":1,"files_number":8,"total_number_errors":2,"files_lint_results":[{"Secret":"never display"}]},
            {"name":"JAVA_PMD","status":"success","return_code":0,"files_number":0},
            {"name":"DISABLED","is_active":false,"status":"success"}
        ]});
        let statuses = parse_megalinter_json(&report.to_string()).unwrap();
        assert_eq!(statuses.len(), 4);
        assert_eq!(statuses[0].return_code, Some(2));
        assert!(statuses[0]
            .error
            .as_deref()
            .unwrap()
            .contains("coverage is incomplete"));
        assert_eq!(statuses[1].found_errors, 2);
        assert!(!statuses[1]
            .error
            .as_deref()
            .unwrap()
            .contains("never display"));
        assert_eq!(statuses[2].status, "skipped");
        assert_eq!(statuses[3].status, "skipped");
    }

    #[test]
    fn managed_secrets_are_redacted_in_plain_encoded_and_base64_forms() {
        let secret = "token+ with/slash";
        let forms = [
            secret.to_string(),
            base64(secret.as_bytes(), false, true),
            base64(secret.as_bytes(), true, false),
            "token%2B%20with%2Fslash".into(),
            "token%2b+with%2fslash".into(),
        ];
        for form in forms {
            assert_eq!(redact(&form, &[secret, ""]), "[REDACTED]");
        }
        assert_eq!(redact("abc", &["abc"]), "[REDACTED]");
        assert_eq!(redact("YWJjZGU=", &["abcde"]), "[REDACTED]");
    }

    #[test]
    fn secret_fields_and_embedded_messages_never_keep_match_values() {
        let mut report = json!({"results":[{"check_id":"generic-secret","extra":{"message":"found NEVER_PRINT", "lines":"NEVER_PRINT", "metavars":{"$X":{"abstract_content":"NEVER_PRINT"}},"metadata":{"category":"security"}}}],"leaks":[{"RuleID":"generic-api-key","Secret":"NEVER_PRINT","Match":"NEVER_PRINT","Description":"Found NEVER_PRINT in source","snippet":{"text":"NEVER_PRINT"}}]});
        let parsed = parse_grep_json("Semgrep CE", &report.to_string(), &[]).unwrap();
        assert!(!parsed.findings[0].message.contains("NEVER_PRINT"));
        strip_secret_fields(&mut report);
        assert!(!report.to_string().contains("NEVER_PRINT"));
    }

    #[test]
    fn severity_thresholds_plain_messages_and_help_schemes() {
        for (score, expected) in [
            (9.0, Severity::Critical),
            (7.0, Severity::High),
            (4.0, Severity::Medium),
            (0.0, Severity::Low),
        ] {
            assert_eq!(sarif_severity(Some("note"), Some(score)), expected);
        }
        assert_eq!(
            sarif_severity(Some("error"), Some(f64::NAN)),
            Severity::High
        );
        assert_eq!(grep_severity("INFO"), Severity::Info);
        assert_eq!(
            plain("&lt;b&gt;plain&lt;/b&gt; &amp; safe"),
            "plain  & safe"
        );
        assert!(help_url(Some("javascript:alert(1)")).is_none());
        assert!(help_url(Some("file:///x")).is_none());
        assert!(help_url(Some("https://user:pass@example.com/x")).is_none());
        assert!(help_url(Some("http://example.com/x")).is_some());
    }

    #[test]
    fn malformed_oversized_and_utf8_preview_are_bounded() {
        assert!(parse_grep_json("test", "{", &[]).is_err());
        assert!(parse_grep_json("test", "{}", &[]).is_err());
        assert!(parse_megalinter_json("{\"linters\":[{}]}").is_err());
        let value = json!({"results":[],"huge":"x".repeat(MAX_FIELD + 1)});
        assert!(parse_grep_json("test", &value.to_string(), &[]).is_err());
        let preview = preview_text("aæ🙂z", 5);
        assert_eq!(preview.text, "aæ");
        assert!(preview.truncated);
        assert_eq!(preview_text("aæ", 3).text, "aæ");
        assert!(!preview_text("aæ", 3).truncated);
        assert_eq!(preview_text("🙂", 0).text, "");
    }
}
