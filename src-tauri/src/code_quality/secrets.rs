//! Secret storage for Sonar tokens and generated database passwords.
//! Production uses Windows Credential Manager generic credentials on Windows, the freedesktop Secret
//! Service (`secret-tool`) on Linux and the login Keychain (`security`) on macOS (see `secrets_unix`);
//! tests use `MemorySecrets`. There is never a plaintext fallback.
//! Secret values never appear in serializable state, logs, argv or `Debug` output.
use super::types::{CredentialStoreStatus, Result};
#[cfg(test)]
use parking_lot::Mutex;
use sha2::Sha256;
#[cfg(test)]
use std::collections::HashMap;
use std::path::Path;
use uuid::Uuid;

pub trait SecretStore: Send + Sync {
    fn write(&self, target: &str, secret: &str) -> Result<()>;
    fn read(&self, target: &str) -> Result<Option<String>>;
    /// Deleting a missing credential is not an error.
    fn delete(&self, target: &str) -> Result<()>;
    /// Cheap, non-blocking view of whether the backing store works (may be a cached result).
    fn status(&self) -> CredentialStoreStatus {
        CredentialStoreStatus {
            name: "credential store".into(),
            available: true,
            message: None,
        }
    }
    /// Blocking, fresh check run before anything is stored. `Err` names the missing prerequisite.
    fn verify(&self) -> Result<()> {
        Ok(())
    }
}

#[must_use]
pub fn sha256_hex(text: &str) -> String {
    use sha2::Digest;
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Windows paths are case-insensitive; Unix paths are exact (`/repo/A` and `/repo/a` are different folders).
#[must_use]
pub fn path_identity(path: &str) -> String {
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.to_string()
    }
}

/// `OMPManager.CodeQuality/<SHA256(config-root)>/<SHA256(identity)>/<kind>`.
/// `identity` distinguishes the server: the Community resource-set id, or the normalized
/// Enterprise origin plus context path, so one server's token is never reused for another.
#[must_use]
pub fn target_name(config_root: &Path, identity: &str, kind: &str) -> String {
    format!(
        "OMPManager.CodeQuality/{}/{}/{}",
        sha256_hex(&path_identity(&config_root.to_string_lossy())),
        sha256_hex(identity),
        kind
    )
}

/// Random 48-character URL-safe password for the managed PostgreSQL database.
#[must_use]
pub fn random_password() -> String {
    let mut out = String::with_capacity(64);
    for _ in 0..2 {
        out.push_str(&Uuid::new_v4().simple().to_string());
    }
    out.truncate(48);
    out
}

/// Random SonarQube admin password: always 42 characters with upper case, lower case, digit and
/// special character so Sonar's password policy accepts it. Kept at or below 50 characters, which
/// stays under every length cap SonarQube applies (and bcrypt's 72-byte input limit); the two
/// 16-hex-digit random groups carry 120 random bits after the UUID version bits.
#[must_use]
pub fn random_sonar_password() -> String {
    let group = || {
        let mut text = Uuid::new_v4().simple().to_string();
        text.truncate(16);
        text
    };
    format!("Omp-{}-{}-Aa1!", group(), group())
}

#[cfg(test)]
#[derive(Default)]
pub struct MemorySecrets {
    values: Mutex<HashMap<String, String>>,
}

#[cfg(test)]
impl SecretStore for MemorySecrets {
    fn write(&self, target: &str, secret: &str) -> Result<()> {
        self.values
            .lock()
            .insert(target.to_string(), secret.to_string());
        Ok(())
    }
    fn read(&self, target: &str) -> Result<Option<String>> {
        Ok(self.values.lock().get(target).cloned())
    }
    fn delete(&self, target: &str) -> Result<()> {
        self.values.lock().remove(target);
        Ok(())
    }
}

#[cfg(windows)]
use windows_sys::Win32::Foundation;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, FILETIME};
#[cfg(windows)]
use windows_sys::Win32::Security::Credentials;
#[cfg(windows)]
use windows_sys::Win32::Security::Credentials::{
    CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
};

#[cfg(windows)]
#[derive(Default)]
pub struct WindowsSecrets;

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
impl SecretStore for WindowsSecrets {
    fn status(&self) -> CredentialStoreStatus {
        CredentialStoreStatus {
            name: "Windows Credential Manager".into(),
            available: true,
            message: None,
        }
    }
    fn write(&self, target: &str, secret: &str) -> Result<()> {
        let mut name = wide(target);
        let mut user = wide("omp-manager");
        let mut blob: Vec<u8> = secret.as_bytes().to_vec();
        let credential = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: name.as_mut_ptr(),
            Comment: std::ptr::null_mut(),
            LastWritten: FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: std::ptr::null_mut(),
            UserName: user.as_mut_ptr(),
        };
        // Win32 has no safe credential-store API; all backing buffers stay alive through the call.
        // SAFETY: credential's pointers refer to the live, terminated UTF-16 names and sized blob.
        let ok = unsafe { Credentials::CredWriteW(&credential, 0) };
        let error = std::io::Error::last_os_error();
        blob.fill(0);
        if ok == 0 {
            return Err(format!(
                "Windows Credential Manager could not save the credential: {error}"
            ));
        }
        Ok(())
    }
    fn read(&self, target: &str) -> Result<Option<String>> {
        let name = wide(target);
        let mut found: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: name is terminated UTF-16, and found is a valid output pointer for CredReadW.
        let ok = unsafe { Credentials::CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut found) };
        if ok == 0 {
            // SAFETY: GetLastError has no preconditions and immediately follows the failed API.
            let code = unsafe { Foundation::GetLastError() };
            if code == ERROR_NOT_FOUND {
                return Ok(None);
            }
            return Err(format!(
                "Windows Credential Manager could not read the credential: {}",
                std::io::Error::from_raw_os_error(code as i32)
            ));
        }
        // SAFETY: successful CredReadW owns a live credential until CredFree; nonempty blobs
        // have the reported readable length. Handle empty blobs without constructing a null slice.
        let text = unsafe {
            let credential = &*found;
            let bytes = if credential.CredentialBlobSize == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(
                    credential.CredentialBlob,
                    credential.CredentialBlobSize as usize,
                )
                .to_vec()
            };
            let text = String::from_utf8(bytes);
            Credentials::CredFree(found as *const _);
            text
        };
        text.map(Some)
            .map_err(|_| "Stored credential is not valid text".to_string())
    }
    fn delete(&self, target: &str) -> Result<()> {
        let name = wide(target);
        // SAFETY: name is terminated UTF-16 and the remaining arguments are fixed Win32 flags.
        if unsafe { Credentials::CredDeleteW(name.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
            // SAFETY: GetLastError has no preconditions and immediately follows the failed API.
            let code = unsafe { Foundation::GetLastError() };
            if code == ERROR_NOT_FOUND {
                return Ok(());
            }
            return Err(format!(
                "Windows Credential Manager could not delete the credential: {}",
                std::io::Error::from_raw_os_error(code as i32)
            ));
        }
        Ok(())
    }
}

#[cfg(all(windows, test))]
mod windows_tests {
    use super::*;

    #[test]
    fn credential_manager_round_trip() {
        let store = WindowsSecrets;
        let target = format!("OMPManager.CodeQuality.test/{}", Uuid::new_v4());
        assert_eq!(store.read(&target).unwrap(), None);
        store.write(&target, "squ_round_trip").unwrap();
        assert_eq!(
            store.read(&target).unwrap().as_deref(),
            Some("squ_round_trip")
        );
        store.write(&target, "squ_replaced").unwrap();
        assert_eq!(
            store.read(&target).unwrap().as_deref(),
            Some("squ_replaced")
        );
        store.write(&target, "").unwrap();
        assert_eq!(
            store.read(&target).unwrap().as_deref(),
            Some(""),
            "empty blobs have no readable pointer"
        );
        store.delete(&target).unwrap();
        assert_eq!(store.read(&target).unwrap(), None);
        store.delete(&target).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_names_separate_servers_and_roots() {
        let root = Path::new("C:/cfg");
        let a = target_name(root, "https://sonar.example/a", "token");
        let b = target_name(root, "https://sonar.example/b", "token");
        let c = target_name(Path::new("C:/other"), "https://sonar.example/a", "token");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("OMPManager.CodeQuality/") && a.ends_with("/token"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_config_roots_differing_only_in_case_never_share_credential_names() {
        let (upper, lower) = (Path::new("/cfg/A"), Path::new("/cfg/a"));
        assert_ne!(
            target_name(upper, "community:set", "token"),
            target_name(lower, "community:set", "token")
        );
        assert_eq!(
            target_name(upper, "community:set", "token"),
            target_name(Path::new("/cfg/A"), "community:set", "token")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_config_roots_keep_case_insensitive_credential_names() {
        assert_eq!(
            target_name(Path::new("C:/Cfg"), "community:set", "token"),
            target_name(Path::new("c:/cfg"), "community:set", "token")
        );
    }

    #[test]
    fn passwords_are_long_and_unique() {
        let (a, b) = (random_password(), random_password());
        assert!(a.len() >= 32 && a != b);
    }

    #[test]
    fn sonar_passwords_satisfy_the_policy_and_are_unique() {
        let (a, b) = (random_sonar_password(), random_sonar_password());
        assert_ne!(a, b);
        for password in [&a, &b] {
            assert!((12..=50).contains(&password.len()), "{}", password.len());
            assert!(password.chars().any(|c| c.is_ascii_uppercase()));
            assert!(password.chars().any(|c| c.is_ascii_lowercase()));
            assert!(password.chars().any(|c| c.is_ascii_digit()));
            assert!(password.chars().any(|c| !c.is_ascii_alphanumeric()));
            assert!(password.chars().all(|c| c.is_ascii_graphic()));
        }
    }
}
