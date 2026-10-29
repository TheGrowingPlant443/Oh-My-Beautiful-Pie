//! Self-contained, script-free HTML rendering of a stored scan. Every dynamic string is escaped; the
//! document loads nothing external (CSP `default-src 'none'`) and links only `https://` help URLs.
use super::types::{
    QualityFinding, QualityGate, QualityMode, QualityScan, QualityScanStatus, Severity, ToolStatus,
};
use std::fmt::Write as _;

const SEVERITIES: [(Severity, &str); 5] = [
    (Severity::Critical, "Critical"),
    (Severity::High, "High"),
    (Severity::Medium, "Medium"),
    (Severity::Low, "Low"),
    (Severity::Info, "Info"),
];

const STYLE: &str = "body{font:14px/1.5 system-ui,Segoe UI,sans-serif;margin:2rem auto;max-width:70rem;padding:0 1rem;color:#1b1f24;background:#fff}\
h1{margin-bottom:.25rem}h2{margin-top:2rem;border-bottom:1px solid #d0d7de;padding-bottom:.25rem}\
table{border-collapse:collapse;width:100%;margin:.5rem 0}th,td{border:1px solid #d0d7de;padding:.3rem .5rem;text-align:left;vertical-align:top}\
th{background:#f6f8fa}code{font-family:Consolas,monospace;word-break:break-all}\
.banner{padding:.75rem 1rem;border-radius:6px;font-weight:600;border:1px solid}\
.ok{background:#dafbe1;border-color:#4ac26b}.warn{background:#fff8c5;border-color:#d4a72c}.err{background:#ffebe9;border-color:#ff8182}.dim{background:#f6f8fa;border-color:#d0d7de}\
.muted{color:#57606a}";

/// Escapes text for HTML element content and quoted attribute values.
#[must_use]
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if c.is_control() && c != '\n' && c != '\t' => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

fn banner(scan: &QualityScan) -> (&'static str, String) {
    let count = scan.findings.len();
    match scan.status {
        QualityScanStatus::Complete if count == 0 => (
            "ok",
            "Completed with zero findings in the analyzed coverage".into(),
        ),
        QualityScanStatus::Complete => (
            "ok",
            format!(
                "Completed with {count} finding{}",
                if count == 1 { "" } else { "s" }
            ),
        ),
        QualityScanStatus::Running => ("dim", "Running - results are not final".into()),
        QualityScanStatus::Cancelling => (
            "warn",
            "Cancelling - analysis incomplete, not a clean result".into(),
        ),
        QualityScanStatus::Cancelled => (
            "warn",
            "Cancelled - analysis incomplete, not a clean result".into(),
        ),
        QualityScanStatus::Partial => (
            "warn",
            "Partial - coverage incomplete, not a clean result".into(),
        ),
        QualityScanStatus::Interrupted => (
            "warn",
            "Interrupted - analysis incomplete, not a clean result".into(),
        ),
        QualityScanStatus::Error => ("err", "Failed - not a clean result".into()),
    }
}

fn backend(mode: QualityMode) -> &'static str {
    match mode {
        QualityMode::SonarqubeCommunity => "SonarQube Community Build (app-owned local server)",
        QualityMode::SonarqubeEnterprise => "SonarQube (connected enterprise server)",
        QualityMode::Alternate => "Alternate analyzers (no SonarQube)",
    }
}

fn tool_status(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Complete => "Complete",
        ToolStatus::Partial => "Partial - coverage incomplete",
        ToolStatus::Error => "Error - no reliable coverage",
        ToolStatus::Skipped => "Skipped - no coverage from this tool",
    }
}

/// `YYYY-MM-DD HH:MM:SS UTC` from epoch milliseconds (proleptic Gregorian, Hinnant's civil-from-days).
fn format_time(millis: u64) -> String {
    let secs = millis / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn safe_help_url(url: &str) -> bool {
    url.starts_with("https://")
        && url.len() <= 2048
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn row(out: &mut String, label: &str, value: &str) {
    let _ = writeln!(
        out,
        "<tr><th scope=\"row\">{}</th><td>{}</td></tr>",
        escape_html(label),
        escape_html(value)
    );
}

fn render_head(out: &mut String, scan: &QualityScan) {
    let folder_name = scan
        .folder
        .trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or(&scan.folder);
    let (tone, banner_text) = banner(scan);
    out.push_str("<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n");
    out.push_str("<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n");
    let _ = writeln!(
        out,
        "<title>Code quality report - {}</title>\n<style>{STYLE}</style></head><body>",
        escape_html(folder_name)
    );
    let _ = writeln!(
        out,
        "<h1>Code quality report: {}</h1>",
        escape_html(folder_name)
    );
    let _ = writeln!(
        out,
        "<div class=\"banner {tone}\" role=\"status\">{}</div>",
        escape_html(&banner_text)
    );
    if let Some(error) = &scan.error {
        let _ = writeln!(out, "<p class=\"err banner\">{}</p>", escape_html(error));
    }
}

fn render_provenance(out: &mut String, scan: &QualityScan) {
    out.push_str("<h2>Provenance</h2>\n<table>\n");
    row(out, "Folder", &scan.folder);
    row(out, "Backend", backend(scan.config.mode));
    row(out, "Container runtime", scan.config.runtime.label());
    row(out, "Started", &format_time(scan.started_at));
    row(
        out,
        "Finished",
        &scan
            .finished_at
            .map(format_time)
            .unwrap_or_else(|| "not finished".into()),
    );
    row(out, "Project key", &scan.project.project_key);
    row(out, "Snapshot hash", &scan.snapshot_hash);
    row(out, "Rules hash", &scan.rules_hash);
    out.push_str("</table>\n");
}

fn render_gate_and_counts(out: &mut String, scan: &QualityScan) {
    out.push_str("<h2>Quality gate</h2>\n");
    let gate = match scan.quality_gate {
        Some(QualityGate::Passed) => "Passed",
        Some(QualityGate::Failed) => "Failed",
        Some(QualityGate::Unknown) => "Unknown",
        None => "Not evaluated",
    };
    let _ = writeln!(out, "<p>{gate}</p>");

    out.push_str(
        "<h2>Findings by severity</h2>\n<table>\n<tr><th>Severity</th><th>Count</th></tr>\n",
    );
    for (severity, label) in SEVERITIES {
        let n = scan
            .findings
            .iter()
            .filter(|f| f.severity == severity)
            .count();
        let _ = writeln!(out, "<tr><th scope=\"row\">{label}</th><td>{n}</td></tr>");
    }
    out.push_str("</table>\n");
}

fn render_tools(out: &mut String, scan: &QualityScan) {
    out.push_str("<h2>Tools</h2>\n");
    if scan.tools.is_empty() {
        out.push_str("<p class=\"muted\">No tool reported an outcome.</p>\n");
        return;
    }
    out.push_str("<table>\n<tr><th>Tool</th><th>Version</th><th>Outcome</th><th>Files analyzed</th><th>Findings</th><th>Error</th></tr>\n");
    for tool in &scan.tools {
        let _ = writeln!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape_html(&tool.tool),
            escape_html(&tool.version),
            tool_status(tool.status),
            tool.analyzed_files
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into()),
            tool.finding_count,
            escape_html(tool.error.as_deref().unwrap_or("")),
        );
    }
    out.push_str("</table>\n");
}

fn render_warnings(out: &mut String, scan: &QualityScan) {
    if scan.warnings.is_empty() {
        return;
    }
    out.push_str("<h2>Warnings</h2>\n<ul>\n");
    for warning in &scan.warnings {
        let _ = writeln!(out, "<li>{}</li>", escape_html(warning));
    }
    out.push_str("</ul>\n");
}

fn finding_location(finding: &QualityFinding) -> String {
    match (&finding.path, finding.line) {
        (Some(path), Some(line)) => format!("{}:{line}", escape_html(path)),
        (Some(path), None) => escape_html(path),
        (None, _) => "(no file)".into(),
    }
}

fn render_finding_row(out: &mut String, finding: &QualityFinding) {
    let rule = escape_html(&finding.rule_id);
    let rule_cell = match finding.help_url.as_deref().filter(|u| safe_help_url(u)) {
        Some(url) => format!(
            "<a href=\"{}\" rel=\"noopener noreferrer\">{rule}</a>",
            escape_html(url)
        ),
        None => rule,
    };
    let location = finding_location(finding);
    let hotspot = if finding.hotspot {
        " <em>(security hotspot)</em>"
    } else {
        ""
    };
    let _ = writeln!(
        out,
        "<tr><td>{rule_cell}</td><td><code>{location}</code></td><td>{}</td><td>{}{hotspot}</td></tr>",
        escape_html(&finding.tools.join(" + ")),
        escape_html(&finding.message),
    );
}

fn render_findings(out: &mut String, scan: &QualityScan) {
    out.push_str("<h2>Findings</h2>\n");
    if scan.findings.is_empty() {
        let text = if scan.status == QualityScanStatus::Complete {
            "No findings in the analyzed coverage."
        } else {
            "No findings were recorded, but the analysis was not complete."
        };
        let _ = writeln!(out, "<p>{text}</p>");
    }
    for (severity, label) in SEVERITIES {
        let mut group: Vec<&QualityFinding> = scan
            .findings
            .iter()
            .filter(|f| f.severity == severity)
            .collect();
        if group.is_empty() {
            continue;
        }
        group.sort_by(|a, b| (&a.path, a.line, &a.rule_id).cmp(&(&b.path, b.line, &b.rule_id)));
        let _ = writeln!(out, "<h3>{label} ({})</h3>\n<table>\n<tr><th>Rule</th><th>Location</th><th>Tools</th><th>Message</th></tr>", group.len());
        for finding in group {
            render_finding_row(out, finding);
        }
        out.push_str("</table>\n");
    }
}

#[must_use]
pub fn render_html(scan: &QualityScan) -> String {
    let mut out = String::with_capacity(4096 + scan.findings.len() * 400);
    render_head(&mut out, scan);
    render_provenance(&mut out, scan);
    render_gate_and_counts(&mut out, scan);
    render_tools(&mut out, scan);
    render_warnings(&mut out, scan);
    render_findings(&mut out, scan);
    out.push_str("</body></html>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::super::types::{QualityConfig, QualityProject, QualityToolOutcome, Scanner};
    use super::*;

    fn finding(
        severity: Severity,
        rule: &str,
        message: &str,
        path: Option<&str>,
        help: Option<&str>,
    ) -> QualityFinding {
        QualityFinding {
            id: format!("{rule}-{message}"),
            tools: vec!["sonar".into(), "other".into()],
            rule_id: rule.into(),
            category: "bug".into(),
            severity,
            message: message.into(),
            path: path.map(Into::into),
            line: Some(7),
            column: None,
            end_line: None,
            help_url: help.map(Into::into),
            hotspot: false,
        }
    }

    fn scan(status: QualityScanStatus, findings: Vec<QualityFinding>) -> QualityScan {
        QualityScan {
            id: "scan-1".into(),
            folder: r"C:\work\demo".into(),
            config: QualityConfig::default(),
            project: QualityProject {
                folder: r"C:\work\demo".into(),
                project_key: "omp-demo".into(),
                scanner: Scanner::Cli,
                entry_file: None,
                image: None,
                build_args: vec![],
                prepare_npm_dependencies: false,
                exclusions: vec![],
            },
            status,
            phase: "Done".into(),
            started_at: 1_700_000_000_000,
            finished_at: Some(1_700_000_060_000),
            snapshot_hash: "snap123".into(),
            rules_hash: "rules456".into(),
            tools: vec![
                QualityToolOutcome {
                    tool: "sonar".into(),
                    version: "1".into(),
                    status: ToolStatus::Complete,
                    exit_code: Some(0),
                    analyzed_files: Some(12),
                    finding_count: 3,
                    error: None,
                    report_names: vec![],
                },
                QualityToolOutcome {
                    tool: "other".into(),
                    version: "2".into(),
                    status: ToolStatus::Skipped,
                    exit_code: None,
                    analyzed_files: None,
                    finding_count: 0,
                    error: None,
                    report_names: vec![],
                },
            ],
            findings,
            warnings: vec!["a <b>warning</b>".into()],
            error: None,
            analysis_id: None,
            dashboard_url: Some("http://127.0.0.1:9000/dashboard".into()),
            quality_gate: Some(QualityGate::Passed),
            log: vec![],
        }
    }

    #[test]
    fn hostile_text_is_escaped_and_nothing_external_or_executable_is_emitted() {
        let evil = "<script>alert(1)</script>";
        let mut s = scan(
            QualityScanStatus::Complete,
            vec![finding(
                Severity::High,
                evil,
                evil,
                Some(evil),
                Some("https://rules.example/x\"onmouseover=\"a"),
            )],
        );
        s.error = Some(evil.into());
        s.project.project_key = evil.into();
        let html = render_html(&s);
        assert!(!html.contains("<script"));
        assert!(!html.contains("alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("src="));
        assert!(!html.contains("http://"));
        assert!(!html.contains("onmouseover=\"a"));
        assert!(html.contains("default-src 'none'"));
        assert_eq!(escape_html("&<>\"'"), "&amp;&lt;&gt;&quot;&#39;");
    }

    #[test]
    fn severity_counts_and_grouping_match_the_findings() {
        let findings = vec![
            finding(Severity::Low, "r-low", "m1", Some("a.ts"), None),
            finding(Severity::Critical, "r-c1", "m2", Some("b.ts"), None),
            finding(Severity::Critical, "r-c2", "m3", None, None),
        ];
        let html = render_html(&scan(QualityScanStatus::Complete, findings));
        assert!(html.contains("<tr><th scope=\"row\">Critical</th><td>2</td></tr>"));
        assert!(html.contains("<tr><th scope=\"row\">High</th><td>0</td></tr>"));
        assert!(html.contains("<tr><th scope=\"row\">Low</th><td>1</td></tr>"));
        assert!(html.find("Critical (2)").unwrap() < html.find("Low (1)").unwrap());
        assert!(html.contains("sonar + other"));
        assert!(html.contains("<code>b.ts:7</code>"));
        assert!(html.contains("(no file)"));
        assert!(html.contains("Completed with 3 findings"));
    }

    #[test]
    fn only_complete_zero_finding_scans_are_worded_as_clean() {
        let clean = render_html(&scan(QualityScanStatus::Complete, vec![]));
        assert!(clean.contains("Completed with zero findings in the analyzed coverage"));
        for status in [
            QualityScanStatus::Partial,
            QualityScanStatus::Interrupted,
            QualityScanStatus::Cancelled,
            QualityScanStatus::Error,
        ] {
            let html = render_html(&scan(status, vec![]));
            assert!(!html.contains("zero findings"), "{status:?}");
            assert!(html.contains("not a clean result"), "{status:?}");
            assert!(
                !html.contains("No findings in the analyzed coverage"),
                "{status:?}"
            );
        }
        assert!(clean.contains("Skipped - no coverage from this tool"));
    }

    #[test]
    fn help_links_require_https() {
        let ok = render_html(&scan(
            QualityScanStatus::Complete,
            vec![finding(
                Severity::Info,
                "r1",
                "m",
                None,
                Some("https://rules.example/r1"),
            )],
        ));
        assert!(
            ok.contains("<a href=\"https://rules.example/r1\" rel=\"noopener noreferrer\">r1</a>")
        );
        for url in [
            "http://rules.example/r1",
            "javascript:alert(1)",
            "//rules.example",
            "https://a b",
        ] {
            let html = render_html(&scan(
                QualityScanStatus::Complete,
                vec![finding(Severity::Info, "r1", "m", None, Some(url))],
            ));
            assert!(!html.contains("<a "), "{url}");
            assert!(!html.contains("javascript:"), "{url}");
        }
    }

    #[test]
    fn timestamps_render_in_utc() {
        assert_eq!(format_time(1_700_000_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(format_time(0), "1970-01-01 00:00:00 UTC");
    }
}
