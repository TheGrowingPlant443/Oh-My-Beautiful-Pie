//! Explicit, local-only credential reveal. Never included in normal state or report serialization.
use super::types::{QualityMode, QualityPhase, Result};
use super::{secrets, sonar, CodeQualityService};
use serde::Serialize;
use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityLoginDetails {
    pub server_url: String,
    pub login: Option<String>,
    pub password: Option<String>,
    pub token: Option<String>,
    pub notice: Option<String>,
}

impl Debug for QualityLoginDetails {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("QualityLoginDetails")
            .field("server_url", &"[redacted]")
            .field("login", &"[redacted]")
            .field("password", &"[redacted]")
            .field("token", &"[redacted]")
            .finish()
    }
}

const MISSING: &str = "Not stored; finish setup or supply a token in Advanced settings.";
const SSO: &str =
    "Enterprise sign-in uses your SSO account; OMP Manager does not store its password.";

impl CodeQualityService {
    /// The ordinary operation gate prevents a concurrent runtime switch or ownership transfer.
    /// Dropping it restores phase/error; this read never starts an engine or stores revealed data.
    pub fn reveal_credentials(self: &Arc<Self>) -> Result<QualityLoginDetails> {
        let _gate = self.admit(QualityPhase::Idle, None)?;
        let config = self.inner.lock().file.config.clone();
        match config.mode {
            QualityMode::SonarqubeCommunity => {
                let set = self.resource_set_id(&config.runtime);
                let password = match set.as_deref() {
                    Some(set) => self.secrets.read(&secrets::target_name(
                        &self.config_root,
                        &format!("community:{set}"),
                        "adminpassword",
                    ))?,
                    None => None,
                };
                let token = match self.community_token_target(&config.runtime) {
                    Some(target) => self.secrets.read(&target)?,
                    None => None,
                };
                Ok(QualityLoginDetails {
                    server_url: sonar::community_url(config.community_port),
                    login: password.as_ref().map(|_| "admin".to_string()),
                    notice: (password.is_none() || token.is_none()).then(|| MISSING.to_string()),
                    password,
                    token,
                })
            }
            QualityMode::SonarqubeEnterprise => {
                let token = if config.enterprise_url.is_empty() {
                    None
                } else {
                    self.secrets
                        .read(&self.enterprise_token_target(&config.enterprise_url))?
                };
                Ok(QualityLoginDetails {
                    server_url: config.enterprise_url,
                    login: None,
                    password: None,
                    notice: Some(if token.is_none() {
                        format!("{SSO} {MISSING}")
                    } else {
                        SSO.to_string()
                    }),
                    token,
                })
            }
            QualityMode::Alternate => Err("This analyzer has no SonarQube credentials.".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_quality::process_lock::{self, ProcessLock};
    use crate::code_quality::runtime::{Engine, EngineFactory};
    use crate::code_quality::secrets::{MemorySecrets, SecretStore};
    use crate::code_quality::store;
    use crate::code_quality::types::{
        OwnershipStatus, QualityOptions, QualityRuntime, BUSY_MESSAGE,
    };
    use std::path::PathBuf;

    struct NoEngine;
    impl EngineFactory for NoEngine {
        fn engine(&self, _: &QualityRuntime) -> Result<Arc<dyn Engine>> {
            panic!("revealing credentials must not access an engine")
        }
    }
    struct Harness {
        svc: Arc<CodeQualityService>,
        root: PathBuf,
        secrets: Arc<MemorySecrets>,
    }
    impl Drop for Harness {
        fn drop(&mut self) {
            self.svc.shutdown();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn harness() -> Harness {
        let root =
            std::env::temp_dir().join(format!("omp-quality-credentials-{}", uuid::Uuid::new_v4()));
        let paths = store::Paths::new(root.join("code-quality"));
        let mut file = store::ConfigFile::default();
        file.config.runtime = QualityRuntime::DockerDesktop;
        store::save_config(&paths, &file).unwrap();
        let mut resources = store::ResourceFile::new();
        resources.runtimes.push(store::RuntimeRecord {
            runtime: QualityRuntime::DockerDesktop,
            fingerprint: "first-engine".into(),
            possibly_running: false,
            port: 9000,
        });
        store::save_resources(&paths, &resources).unwrap();
        let lock = ProcessLock::acquire(&process_lock::name_for(&paths.root));
        let secrets = Arc::new(MemorySecrets::default());
        let svc = CodeQualityService::with_parts(
            root.clone(),
            Arc::new(NoEngine),
            secrets.clone(),
            lock,
            Arc::new(|_, _| (vec![], QualityOptions::default())),
        );
        Harness { svc, root, secrets }
    }

    #[test]
    fn community_reveal_uses_current_engine_identity_and_redacts_diagnostics() {
        let h = harness();
        let set = h
            .svc
            .resource_set_id(&QualityRuntime::DockerDesktop)
            .unwrap();
        let target = secrets::target_name(&h.root, &format!("community:{set}"), "adminpassword");
        h.secrets.write(&target, "private-password-first").unwrap();
        h.secrets
            .write(
                &h.svc
                    .community_token_target(&QualityRuntime::DockerDesktop)
                    .unwrap(),
                "private-token-first",
            )
            .unwrap();
        let details = h.svc.reveal_credentials().unwrap();
        assert_eq!(details.login.as_deref(), Some("admin"));
        assert_eq!(details.password.as_deref(), Some("private-password-first"));
        assert_eq!(details.token.as_deref(), Some("private-token-first"));
        assert!(details.notice.is_none());
        for text in [
            format!("{details:?}"),
            serde_json::to_string(&h.svc.state()).unwrap(),
            serde_json::to_string(&h.svc.inner.lock().file).unwrap(),
        ] {
            assert!(
                !text.contains("private-password-first") && !text.contains("private-token-first"),
                "credential material must stay out of diagnostics and persisted state"
            );
        }
        let mut resources = store::load_resources(&h.svc.paths).unwrap();
        resources.runtimes[0].fingerprint = "replacement-engine".into();
        store::save_resources(&h.svc.paths, &resources).unwrap();
        let replaced = h.svc.reveal_credentials().unwrap();
        assert!(
            replaced.password.is_none() && replaced.login.is_none() && replaced.token.is_none()
        );
        assert_eq!(replaced.notice.as_deref(), Some(MISSING));
    }

    #[test]
    fn enterprise_reveal_never_claims_an_sso_password_or_another_servers_token() {
        let h = harness();
        {
            let mut inner = h.svc.inner.lock();
            inner.file.config.mode = QualityMode::SonarqubeEnterprise;
            inner.file.config.enterprise_url = "https://sonar.example/team".into();
        }
        h.secrets
            .write(
                &h.svc.enterprise_token_target("https://sonar.example/team"),
                "private-enterprise-token",
            )
            .unwrap();
        let details = h.svc.reveal_credentials().unwrap();
        assert!(details.password.is_none() && details.login.is_none());
        assert_eq!(details.token.as_deref(), Some("private-enterprise-token"));
        assert_eq!(details.notice.as_deref(), Some(SSO));
        h.svc.inner.lock().file.config.enterprise_url = "https://sonar.example/other".into();
        let other = h.svc.reveal_credentials().unwrap();
        assert!(other.token.is_none());
        assert!(other.notice.unwrap().contains(MISSING));
    }

    #[test]
    fn waiting_process_cannot_reveal_owners_credentials() {
        let h = harness();
        let waiter = CodeQualityService::with_parts(
            h.root.clone(),
            Arc::new(NoEngine),
            h.secrets.clone(),
            Ok(None),
            Arc::new(|_, _| (vec![], QualityOptions::default())),
        );
        assert!(waiter.reveal_credentials().is_err());
        assert!(!waiter.state().busy);
        waiter.shutdown();
        assert_eq!(h.svc.state().ownership.status, OwnershipStatus::Owned);
    }

    #[test]
    fn reveal_refuses_busy_gate_and_alternate_analyzer_without_starting_anything() {
        let h = harness();
        let gate = h.svc.admit(QualityPhase::Scanning, None).unwrap();
        assert_eq!(h.svc.reveal_credentials().unwrap_err(), BUSY_MESSAGE);
        drop(gate);
        h.svc.inner.lock().file.config.mode = QualityMode::Alternate;
        assert_eq!(
            h.svc.reveal_credentials().unwrap_err(),
            "This analyzer has no SonarQube credentials."
        );
    }
}
