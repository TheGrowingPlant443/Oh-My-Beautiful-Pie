//! Cross-process ownership of one code-quality root. A Windows named mutex is held by a dedicated
//! thread (mutexes are thread-affine) until the lock is dropped; a second owner is reported as `None`.
//! On Unix the same authority is an exclusive non-blocking `flock(2)` on `<root>/process.lock`: the
//! kernel releases it when the owning process exits or crashes, so a waiting window recovers.
//! Windows named mutexes have no standard-library equivalent. Unix uses `File::try_lock` and an
//! explicit unlock before closing, so a forked child's inherited descriptor cannot retain authority.
#[cfg(windows)]
use super::secrets;
use super::types::Result;
#[cfg(unix)]
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

#[must_use]
pub struct ProcessLock {
    release: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
    /// The owned description is explicitly unlocked before it is closed on drop.
    #[cfg(unix)]
    _file: Option<File>,
}

#[must_use]
#[cfg(windows)]
pub fn name_for(root: &Path) -> String {
    format!(
        "Local\\OMPManager.CodeQuality.{}",
        &secrets::sha256_hex(&root.to_string_lossy().to_lowercase())[..32]
    )
}

/// The lock identity on Unix is the lock file path itself.
#[cfg(unix)]
#[must_use]
pub fn name_for(root: &Path) -> String {
    root.join("process.lock").to_string_lossy().into_owned()
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        self.release.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // A concurrently forked child can still share this description until exec closes it.
        #[cfg(unix)]
        if let Some(file) = &self._file {
            let _ = file.unlock();
        }
    }
}

impl ProcessLock {
    #[cfg(test)]
    /// A lock that guards nothing; used by tests.
    pub fn unowned() -> ProcessLock {
        ProcessLock {
            release: None,
            thread: None,
            #[cfg(unix)]
            _file: None,
        }
    }

    /// `Ok(None)` when another holder owns the mutex.
    #[cfg(windows)]
    pub fn acquire(name: &str) -> Result<Option<ProcessLock>> {
        use std::thread::Builder;
        use windows_sys::Win32::Foundation::{self, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading;
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel::<Result<bool>>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let thread = Builder::new()
            .name("code-quality-process-lock".into())
            .spawn(move || {
                // SAFETY: `wide` is NUL-terminated and lives until creation returns; no attributes are supplied.
                let handle = unsafe { Threading::CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
                if handle.is_null() {
                    let _ = acquired_tx.send(Err(format!(
                        "Could not create the code quality lock: {}",
                        std::io::Error::last_os_error()
                    )));
                    return;
                }
                // SAFETY: creation returned a live handle, kept open until after this nonblocking wait.
                let waited = unsafe { Threading::WaitForSingleObject(handle, 0) };
                if waited == WAIT_OBJECT_0 || waited == WAIT_ABANDONED {
                    let _ = acquired_tx.send(Ok(true));
                    let _ = release_rx.recv();
                    // SAFETY: a successful or abandoned wait granted this thread ownership.
                    unsafe { Threading::ReleaseMutex(handle) };
                } else if waited == WAIT_TIMEOUT {
                    let _ = acquired_tx.send(Ok(false));
                } else {
                    let _ = acquired_tx.send(Err(format!(
                        "Could not inspect the code quality lock: {}",
                        std::io::Error::last_os_error()
                    )));
                }
                // SAFETY: this thread owns the live handle and closes it exactly once after its last use.
                unsafe { Foundation::CloseHandle(handle) };
            })
            .map_err(|e| e.to_string())?;
        match acquired_rx.recv().map_err(|e| e.to_string())? {
            Ok(true) => Ok(Some(ProcessLock {
                release: Some(release_tx),
                thread: Some(thread),
            })),
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

    /// `Ok(None)` when another holder owns the lock file.
    #[cfg(unix)]
    pub fn acquire(name: &str) -> Result<Option<ProcessLock>> {
        use std::fs::TryLockError;
        use std::os::unix::fs::OpenOptionsExt;
        if name.contains('\u{fffd}') {
            return Err("The code quality lock path is not valid UTF-8; use an application data folder with a plain path.".into());
        }
        let path = Path::new(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Could not create the code quality lock folder: {e}"))?;
        }
        // Never follow a planted symlink to lock (and create) a file elsewhere.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|e| {
                format!(
                    "Could not open the code quality lock {}: {e}",
                    path.display()
                )
            })?;
        loop {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(Some(ProcessLock {
                        release: None,
                        thread: None,
                        _file: Some(file),
                    }))
                }
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(TryLockError::Error(error)) if error.raw_os_error() == Some(libc::EINTR) => {
                    continue
                }
                Err(TryLockError::Error(error)) => {
                    return Err(format!("Could not take the code quality lock: {error}"))
                }
            }
        }
    }

    #[cfg(not(any(windows, unix)))]
    pub fn acquire(_name: &str) -> Result<Option<ProcessLock>> {
        Ok(Some(ProcessLock {
            release: None,
            thread: None,
        }))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn second_owner_is_refused_until_release() {
        let name = format!(
            "Local\\OMPManager.CodeQuality.test.{}",
            uuid::Uuid::new_v4().simple()
        );
        let first = ProcessLock::acquire(&name).unwrap().expect("first owner");
        assert!(ProcessLock::acquire(&name).unwrap().is_none());
        drop(first);
        assert!(ProcessLock::acquire(&name).unwrap().is_some());
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    fn scratch() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("omp-lock-{}", uuid::Uuid::new_v4().simple()))
    }

    #[test]
    fn second_owner_is_refused_until_release() {
        let root = scratch();
        let name = name_for(&root);
        let first = ProcessLock::acquire(&name).unwrap().expect("first owner");
        assert!(ProcessLock::acquire(&name).unwrap().is_none());
        drop(first);
        let again = ProcessLock::acquire(&name)
            .unwrap()
            .expect("released lock can be reacquired");
        drop(again);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn locks_are_per_root() {
        let (a, b) = (scratch(), scratch());
        let first = ProcessLock::acquire(&name_for(&a)).unwrap().expect("a");
        let second = ProcessLock::acquire(&name_for(&b))
            .unwrap()
            .expect("b is independent");
        drop((first, second));
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }

    #[test]
    fn refuses_a_symlinked_lock_file() {
        let root = scratch();
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("elsewhere");
        std::fs::write(&target, b"x").unwrap();
        std::os::unix::fs::symlink(&target, root.join("process.lock")).unwrap();
        assert!(ProcessLock::acquire(&name_for(&root)).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dropping_owner_releases_a_description_still_inherited_by_a_child() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        let root = scratch();
        let name = name_for(&root);
        let owner = ProcessLock::acquire(&name).unwrap().unwrap();
        let (mut parent, child) = UnixStream::pair().unwrap();
        // Command would exec and close the descriptor, so only fork reproduces this inheritance.
        // SAFETY: the forked child only closes/reads descriptors and calls _exit; no Rust cleanup runs.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed: {}", std::io::Error::last_os_error());
        if pid == 0 {
            // SAFETY: both streams own live descriptors and `byte` is a valid one-byte buffer;
            // only async-signal-safe libc calls run after fork, then _exit avoids Rust cleanup.
            unsafe {
                libc::close(parent.as_raw_fd());
                let mut byte = 0u8;
                libc::read(child.as_raw_fd(), std::ptr::from_mut(&mut byte).cast(), 1);
                libc::_exit(0);
            }
        }
        drop(child);
        drop(owner);
        let acquired = ProcessLock::acquire(&name);
        let released = parent.write_all(&[1]);
        drop(parent);
        // SAFETY: pid is the unreaped child created above; no status buffer is requested.
        assert_eq!(unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) }, pid);
        released.unwrap();
        let acquired = acquired.unwrap();
        assert!(
            acquired.is_some(),
            "a forked child must not retain a released owner lock"
        );
        drop(acquired);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Child half of the cross-process test: only active when the parent sets `OMP_LOCK_HOLD`.
    #[test]
    #[ignore = "helper process for lock_is_released_when_the_holding_process_is_killed"]
    fn hold_lock_helper() {
        let Ok(name) = std::env::var("OMP_LOCK_HOLD") else {
            return;
        };
        let _lock = ProcessLock::acquire(&name)
            .unwrap()
            .expect("helper takes the free lock");
        println!("HELD");
        std::thread::sleep(std::time::Duration::from_secs(120));
    }

    /// The kernel, not our code, releases the lock when the holding process dies: a separate
    /// process takes the lock and is SIGKILLed; the lock must then be acquirable here.
    #[test]
    fn lock_is_released_when_the_holding_process_is_killed() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let root = scratch();
        let name = name_for(&root);
        let mut holder = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "--nocapture",
                "code_quality::process_lock::unix_tests::hold_lock_helper",
            ])
            .env("OMP_LOCK_HOLD", &name)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(holder.stdout.take().unwrap()).lines();
        loop {
            match lines.next() {
                Some(Ok(line)) if line.trim() == "HELD" => break,
                Some(Ok(_)) => continue,
                other => panic!("helper process did not take the lock: {other:?}"),
            }
        }
        assert!(
            ProcessLock::acquire(&name).unwrap().is_none(),
            "another process owns the lock"
        );
        holder.kill().unwrap();
        let _ = holder.wait();
        let acquired = ProcessLock::acquire(&name).unwrap();
        assert!(
            acquired.is_some(),
            "the lock is released once the holder is gone"
        );
        drop(acquired);
        let _ = std::fs::remove_dir_all(&root);
    }
}
