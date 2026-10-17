//! Secret storage for Sonar tokens and generated database passwords.
//! Production uses Windows Credential Manager generic credentials; tests use `MemorySecrets`.
//! Secret values never appear in serializable state, logs, argv or `Debug` output.
use super::types::Result;
use sha2::{Digest, Sha256};
use std::path::Path;

pub trait SecretStore: Send + Sync {
    fn write(&self, target: &str, secret: &str) -> Result<()>;
    fn read(&self, target: &str) -> Result<Option<String>>;
    /// Deleting a missing credential is not an error.
    fn delete(&self, target: &str) -> Result<()>;
}

pub fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// `OMPManager.CodeQuality/<SHA256(config-root)>/<SHA256(identity)>/<kind>`.
/// `identity` distinguishes the server: the Community resource-set id, or the normalized
/// Enterprise origin plus context path, so one server's token is never reused for another.
pub fn target_name(config_root: &Path, identity: &str, kind: &str) -> String {
    format!(
        "OMPManager.CodeQuality/{}/{}/{}",
        sha256_hex(&config_root.to_string_lossy().to_lowercase()),
        sha256_hex(identity),
        kind
    )
}

/// Random 32-character URL-safe password for the managed PostgreSQL database.
pub fn random_password() -> String {
    let mut out = String::with_capacity(36);
    for _ in 0..2 {
        out.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    out.truncate(48);
    out
}

#[cfg(any(test, not(windows)))]
#[derive(Default)]
pub struct MemorySecrets {
    values: parking_lot::Mutex<std::collections::HashMap<String, String>>,
}

#[cfg(any(test, not(windows)))]
impl SecretStore for MemorySecrets {
    fn write(&self, target: &str, secret: &str) -> Result<()> {
        self.values.lock().insert(target.to_string(), secret.to_string());
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
pub use windows_store::WindowsSecrets;

#[cfg(windows)]
mod windows_store {
    use super::{Result, SecretStore};
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND};
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    #[derive(Default)]
    pub struct WindowsSecrets;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    impl SecretStore for WindowsSecrets {
        fn write(&self, target: &str, secret: &str) -> Result<()> {
            let mut name = wide(target);
            let mut user = wide("omp-manager");
            let mut blob: Vec<u8> = secret.as_bytes().to_vec();
            let credential = CREDENTIALW {
                Flags: 0,
                Type: CRED_TYPE_GENERIC,
                TargetName: name.as_mut_ptr(),
                Comment: std::ptr::null_mut(),
                LastWritten: unsafe { std::mem::zeroed() },
                CredentialBlobSize: blob.len() as u32,
                CredentialBlob: blob.as_mut_ptr(),
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                AttributeCount: 0,
                Attributes: std::ptr::null_mut(),
                TargetAlias: std::ptr::null_mut(),
                UserName: user.as_mut_ptr(),
            };
            let ok = unsafe { CredWriteW(&credential, 0) };
            let error = std::io::Error::last_os_error();
            blob.iter_mut().for_each(|b| *b = 0);
            if ok == 0 {
                return Err(format!("Windows Credential Manager could not save the credential: {error}"));
            }
            Ok(())
        }
        fn read(&self, target: &str) -> Result<Option<String>> {
            let name = wide(target);
            let mut found: *mut CREDENTIALW = std::ptr::null_mut();
            let ok = unsafe { CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut found) };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                if code == ERROR_NOT_FOUND {
                    return Ok(None);
                }
                return Err(format!("Windows Credential Manager could not read the credential: {}", std::io::Error::from_raw_os_error(code as i32)));
            }
            let text = unsafe {
                let credential = &*found;
                let bytes = std::slice::from_raw_parts(credential.CredentialBlob, credential.CredentialBlobSize as usize);
                let text = String::from_utf8(bytes.to_vec());
                CredFree(found as *const _);
                text
            };
            text.map(Some).map_err(|_| "Stored credential is not valid text".to_string())
        }
        fn delete(&self, target: &str) -> Result<()> {
            let name = wide(target);
            if unsafe { CredDeleteW(name.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
                let code = unsafe { GetLastError() };
                if code == ERROR_NOT_FOUND {
                    return Ok(());
                }
                return Err(format!("Windows Credential Manager could not delete the credential: {}", std::io::Error::from_raw_os_error(code as i32)));
            }
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn credential_manager_round_trip() {
            let store = WindowsSecrets;
            let target = format!("OMPManager.CodeQuality.test/{}", uuid::Uuid::new_v4());
            assert_eq!(store.read(&target).unwrap(), None);
            store.write(&target, "squ_round_trip").unwrap();
            assert_eq!(store.read(&target).unwrap().as_deref(), Some("squ_round_trip"));
            store.write(&target, "squ_replaced").unwrap();
            assert_eq!(store.read(&target).unwrap().as_deref(), Some("squ_replaced"));
            store.delete(&target).unwrap();
            assert_eq!(store.read(&target).unwrap(), None);
            store.delete(&target).unwrap();
        }
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

    #[test]
    fn passwords_are_long_and_unique() {
        let (a, b) = (random_password(), random_password());
        assert!(a.len() >= 32 && a != b);
    }
}
