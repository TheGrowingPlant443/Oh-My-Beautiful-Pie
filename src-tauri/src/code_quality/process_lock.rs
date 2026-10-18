//! Cross-process ownership of one code-quality root. A Windows named mutex is held by a dedicated
//! thread (mutexes are thread-affine) until the lock is dropped; a second owner is reported as `None`.
use super::secrets::sha256_hex;
use super::types::Result;
use std::path::Path;

pub struct ProcessLock {
    release: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub fn name_for(root: &Path) -> String {
    format!("Local\\OMPManager.CodeQuality.{}", &sha256_hex(&root.to_string_lossy().to_lowercase())[..32])
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        self.release.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl ProcessLock {
    #[cfg(test)]
    /// A lock that guards nothing; used by tests and non-Windows builds.
    pub fn unowned() -> ProcessLock {
        ProcessLock { release: None, thread: None }
    }

    /// `Ok(None)` when another holder owns the mutex.
    #[cfg(windows)]
    pub fn acquire(name: &str) -> Result<Option<ProcessLock>> {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel::<Result<bool>>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("code-quality-process-lock".into())
            .spawn(move || unsafe {
                let handle = CreateMutexW(std::ptr::null(), 0, wide.as_ptr());
                if handle.is_null() {
                    let _ = acquired_tx.send(Err(format!("Could not create the code quality lock: {}", std::io::Error::last_os_error())));
                    return;
                }
                let waited = WaitForSingleObject(handle, 0);
                if waited == WAIT_OBJECT_0 || waited == WAIT_ABANDONED {
                    let _ = acquired_tx.send(Ok(true));
                    let _ = release_rx.recv();
                    ReleaseMutex(handle);
                } else if waited == WAIT_TIMEOUT {
                    let _ = acquired_tx.send(Ok(false));
                } else {
                    let _ = acquired_tx.send(Err(format!("Could not inspect the code quality lock: {}", std::io::Error::last_os_error())));
                }
                CloseHandle(handle);
            })
            .map_err(|e| e.to_string())?;
        match acquired_rx.recv().map_err(|e| e.to_string())? {
            Ok(true) => Ok(Some(ProcessLock { release: Some(release_tx), thread: Some(thread) })),
            Ok(false) => {
                let _ = thread.join();
                Ok(None)
            }
            Err(e) => {
                let _ = thread.join();
                Err(e)
            }
        }
    }

    #[cfg(not(windows))]
    pub fn acquire(_name: &str) -> Result<Option<ProcessLock>> {
        Ok(Some(ProcessLock { release: None, thread: None }))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn second_owner_is_refused_until_release() {
        let name = format!("Local\\OMPManager.CodeQuality.test.{}", uuid::Uuid::new_v4().simple());
        let first = ProcessLock::acquire(&name).unwrap().expect("first owner");
        assert!(ProcessLock::acquire(&name).unwrap().is_none());
        drop(first);
        assert!(ProcessLock::acquire(&name).unwrap().is_some());
    }
}
