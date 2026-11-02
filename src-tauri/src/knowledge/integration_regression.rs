//! Passive Cognee discovery distinguishes absence from failed inspection. All IO stays in disposable fixtures;
//! injected permission failures are deterministic across platforms and never depend on user stores or privileges.
use super::integrations::{cognee_version, probe_cognee_folder, CogneeProbeError, Environment};
use super::{CogneeOpenState, CogneeState, KnowledgeService, Result};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("omp-cognee-regression-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(crate::setup::canonical(&path).unwrap())
    }

    fn folder(&self) -> PathBuf {
        self.0.join("cognee")
    }
    fn config(&self) -> PathBuf {
        self.0.join("config")
    }
    fn settings(&self) -> PathBuf {
        self.config().join("knowledge/integrations.json")
    }

    fn service(&self) -> Arc<KnowledgeService> {
        KnowledgeService::with_environment(
            self.config(),
            self.0.join("home"),
            Environment {
                cognee_home: Some(self.folder()),
                ..Environment::isolated()
            },
        )
    }

    fn install(&self) {
        let cli = if cfg!(windows) {
            "venv/Scripts/cognee-cli.exe"
        } else {
            "venv/bin/cognee-cli"
        };
        let cli = self.folder().join(cli);
        fs::create_dir_all(cli.parent().unwrap()).unwrap();
        // Deliberately not executable: the probe must never try to run it.
        fs::write(cli, b"not a real executable").unwrap();
        fs::create_dir_all(self.packages().join("cognee-1.6.4.dist-info")).unwrap();
    }

    fn packages(&self) -> PathBuf {
        self.folder().join(if cfg!(windows) {
            "venv/Lib/site-packages"
        } else {
            "venv/lib/python3.12/site-packages"
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn permission_failure(
    _: &Path,
) -> Result<Option<super::integrations::CogneeEvidence>, CogneeProbeError> {
    Err(CogneeProbeError {
        message: io::Error::from(io::ErrorKind::PermissionDenied).to_string(),
        evidence: None,
    })
}

#[test]
fn cognee_missing_is_absent_but_injected_permission_failure_is_unknown() {
    let fixture = Fixture::new();
    assert!(cognee_version(&fixture.folder()).unwrap().is_none());
    assert!(probe_cognee_folder(&fixture.folder()).unwrap().is_none());
    let service = fixture.service();
    let absent = service.probe_cognee();
    assert_eq!(absent.state, CogneeState::Absent);
    assert_eq!(absent.installed, Some(false));
    let unreadable = service.probe_cognee_with(permission_failure);
    assert_eq!(unreadable.state, CogneeState::Unreadable);
    assert_eq!(unreadable.installed, None);
    assert!(unreadable
        .message
        .as_deref()
        .unwrap()
        .contains("could not be fully inspected"));
    let wire = serde_json::to_value(unreadable).unwrap();
    assert_eq!(wire["state"], "unreadable");
    assert!(wire["installed"].is_null());
    assert!(
        !fixture.config().exists(),
        "passive discovery must not create settings"
    );
}

#[test]
fn cognee_failed_reinspection_retains_positive_runtime_evidence() {
    let fixture = Fixture::new();
    fixture.install();
    let service = fixture.service();
    assert_eq!(service.probe_cognee().installed, Some(true));
    let unreadable = service.probe_cognee_with(permission_failure);
    assert_eq!(unreadable.state, CogneeState::Unreadable);
    assert_eq!(unreadable.installed, Some(true));
    assert_eq!(unreadable.version.as_deref(), Some("1.6.4"));
    assert_eq!(
        unreadable.installation_folder.as_deref(),
        Some(fixture.folder().to_string_lossy().as_ref())
    );
    assert!(!fixture.config().exists());
}

#[test]
fn cognee_package_enumeration_errors_are_not_absence_even_with_cli_evidence() {
    let fixture = Fixture::new();
    fixture.install();
    fs::remove_dir_all(fixture.packages()).unwrap();
    fs::write(fixture.packages(), b"not a directory").unwrap();
    assert!(cognee_version(&fixture.folder()).is_err());
    let error = probe_cognee_folder(&fixture.folder()).unwrap_err();
    assert!(
        error.evidence.is_some(),
        "the readable CLI is still positive evidence"
    );
    let service = fixture.service();
    let unreadable = service.probe_cognee();
    assert_eq!(unreadable.state, CogneeState::Unreadable);
    assert_eq!(unreadable.installed, Some(true));
    let opened = service.open_cognee_ui();
    assert_eq!(opened.state, CogneeOpenState::MissingComponent);
    assert!(opened
        .message
        .unwrap()
        .contains("could not be fully inspected"));
    assert!(
        !fixture.config().exists(),
        "an uninspectable installation writes nothing"
    );
}

#[test]
fn cognee_settings_load_failure_is_unknown_not_absent_and_is_not_overwritten() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.settings().parent().unwrap()).unwrap();
    fs::write(fixture.settings(), b"invalid JSON").unwrap();
    let service = fixture.service();
    let unreadable = service.probe_cognee();
    assert_eq!(unreadable.state, CogneeState::Unreadable);
    assert_eq!(unreadable.installed, None);
    assert!(unreadable
        .message
        .unwrap()
        .contains("Integration settings are unreadable"));
    let opened = service.open_cognee_ui();
    assert_eq!(opened.state, CogneeOpenState::MissingComponent);
    assert!(opened.message.unwrap().contains("settings are unreadable"));
    assert_eq!(fs::read(fixture.settings()).unwrap(), b"invalid JSON");
}

#[test]
fn cognee_settings_failure_preserves_known_installation() {
    let fixture = Fixture::new();
    fixture.install();
    let service = fixture.service();
    assert_eq!(service.probe_cognee().installed, Some(true));
    service.state.lock().integrations_error = Some("injected settings read failure".into());
    let unreadable = service.probe_cognee();
    assert_eq!(unreadable.state, CogneeState::Unreadable);
    assert_eq!(unreadable.installed, Some(true));
    assert_eq!(unreadable.version.as_deref(), Some("1.6.4"));
    assert!(unreadable
        .message
        .unwrap()
        .contains("injected settings read failure"));
    assert_eq!(
        service.open_cognee_ui().state,
        CogneeOpenState::MissingComponent
    );
}

#[test]
fn cognee_old_saved_ui_address_and_folder_are_ignored_and_dropped_without_touching_other_settings()
{
    let fixture = Fixture::new();
    fixture.install();
    fs::create_dir_all(fixture.settings().parent().unwrap()).unwrap();
    let elsewhere = fixture.0.join("elsewhere");
    let old = serde_json::json!({
        "version": 1,
        "cognee": { "uiUrl": "http://127.0.0.1:3000/", "installationFolder": elsewhere, "installationEvidence": { "folder": elsewhere, "version": "9.9.9" } },
        "obsidian": { "declinedProjects": ["c:/kept"] },
        "graphifyOutputs": { "c:/project": "c:/out" }
    });
    fs::write(fixture.settings(), serde_json::to_vec(&old).unwrap()).unwrap();
    let service = fixture.service();
    let probe = service.probe_cognee();
    assert_eq!(
        probe.state,
        CogneeState::Installed,
        "a saved folder never replaces the discovered installation"
    );
    assert_eq!(probe.version.as_deref(), Some("1.6.4"));
    assert_eq!(
        probe.installation_folder.as_deref(),
        Some(fixture.folder().to_string_lossy().as_ref())
    );
    let wire = serde_json::to_value(&probe).unwrap();
    assert!(
        wire.get("uiUrl").is_none(),
        "no configured address is reported any more"
    );
    // The next unrelated save drops the obsolete fields and keeps everything else.
    let repository = fixture.0.join("repo");
    fs::create_dir_all(repository.join(".git")).unwrap();
    service
        .record_obsidian_decline(repository.to_string_lossy().as_ref())
        .unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.settings()).unwrap()).unwrap();
    assert_eq!(saved["cognee"], serde_json::json!({}));
    // The set is sorted: on Windows the kept entry sorts first, on Unix the new absolute `/...` path sorts before it.
    #[cfg(windows)]
    assert_eq!(saved["obsidian"]["declinedProjects"][0], "c:/kept");
    #[cfg(unix)]
    {
        let declined = saved["obsidian"]["declinedProjects"].as_array().unwrap();
        assert_eq!(declined.len(), 2, "{declined:?}");
        assert!(
            declined.iter().any(|entry| entry == "c:/kept"),
            "{declined:?}"
        );
    }
    assert_eq!(saved["graphifyOutputs"]["c:/project"], "c:/out");
}
