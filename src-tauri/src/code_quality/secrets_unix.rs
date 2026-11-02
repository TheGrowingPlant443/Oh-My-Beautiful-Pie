//! Credential storage on Linux and macOS through the operating system's own store, reached with a
//! fixed external tool (never plaintext files, never a fallback):
//!
//! * Linux: the freedesktop Secret Service via `secret-tool` (libsecret). The secret travels on the
//!   tool's stdin; argv only carries the non-secret attributes.
//! * macOS: the login Keychain via `security -i`, whose commands are read from stdin so the secret
//!   is not part of any process argument list. (Written from the documented CLI; not run on macOS.)
//!
//! When the tool or the service is missing the store reports a precise, actionable state and every
//! operation fails with that text instead of storing the secret anywhere else.
use super::secrets::SecretStore;
use super::types::{CredentialStoreStatus, Result};
use parking_lot::Mutex;
use std::ffi::OsStr;
#[cfg(test)]
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Attribute value identifying every item this app stores.
const SERVICE: &str = "omp-manager.code-quality";
/// An unlock prompt may legitimately wait for the user; anything longer is a hung service.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(120);
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a successful/failed check is reused by the non-blocking `status()`.
const STATUS_TTL: Duration = Duration::from_secs(30);
const OUTPUT_LIMIT: u64 = 64 * 1024;
/// Environment the Secret Service client needs to find the user's session bus; nothing else is inherited.
const ENV_ALLOWLIST: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "PATH",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "XDG_DATA_DIRS",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "XDG_CONFIG_DIRS",
    "XDG_SESSION_ID",
    "XDG_SESSION_TYPE",
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "WAYLAND_DISPLAY",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// `secret-tool` (libsecret), Linux.
    SecretTool,
    /// `security -i`, macOS.
    MacKeychain,
}

impl Backend {
    fn display(self) -> &'static str {
        match self {
            Backend::SecretTool => "Secret Service (secret-tool)",
            Backend::MacKeychain => "macOS Keychain",
        }
    }
}

struct Output {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

#[derive(Default)]
struct Cache {
    checked: Option<(Instant, Result<()>)>,
}

struct Shared {
    backend: Backend,
    program: Result<PathBuf>,
    cache: Mutex<Cache>,
    refreshing: AtomicBool,
}

pub struct ExternalSecrets {
    shared: Arc<Shared>,
}

/// Looks `name` up in `search` (a PATH-style list), then in the fixed `extra` directories.
pub(crate) fn discover_in(
    name: &str,
    search: Option<&OsStr>,
    extra: &[PathBuf],
) -> Option<PathBuf> {
    let from_path = search.into_iter().flat_map(std::env::split_paths);
    for directory in from_path {
        if !directory.is_absolute() {
            continue;
        }
        let candidate = directory.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    extra
        .iter()
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn standard_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".nix-profile/bin"));
    }
    dirs.extend(
        [
            "/run/current-system/sw/bin",
            "/usr/bin",
            "/usr/local/bin",
            "/opt/homebrew/bin",
        ]
        .map(PathBuf::from),
    );
    dirs
}

/// macOS Keychain command quoting: `security -i` parses double-quoted words with backslash escapes.
fn keychain_word(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn target_label(target: &str) -> String {
    format!(
        "OMP Manager code quality ({})",
        target.rsplit('/').next().unwrap_or("credential")
    )
}

fn bounded(mut reader: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = (&mut reader).take(OUTPUT_LIMIT).read_to_end(&mut bytes);
    let _ = std::io::copy(&mut reader, &mut std::io::sink()); // never block the child on a full pipe
    bytes
}

fn run(program: &Path, args: &[&str], stdin: Option<&[u8]>, timeout: Duration) -> Result<Output> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    let mut child = {
        let mut attempts = 0;
        loop {
            match cmd.spawn() {
                Ok(child) => break child,
                // A freshly written helper can be briefly "text file busy" while another thread forks.
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && attempts < 20 => {
                    attempts += 1;
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(format!("Could not start {}: {e}", program.display())),
            }
        }
    };
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // The secret is small enough for the pipe buffer; a child that exits early only breaks the pipe.
        let _ = pipe.write_all(input);
    }
    let out = child
        .stdout
        .take()
        .map(|s| std::thread::spawn(move || bounded(s)));
    let err = child
        .stderr
        .take()
        .map(|s| std::thread::spawn(move || bounded(s)));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} did not answer within {} seconds (is the keyring locked and waiting for a prompt?).", program.display(), timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("Could not wait for {}: {e}", program.display())),
        }
    };
    let stdout = out.and_then(|t| t.join().ok()).unwrap_or_default();
    let stderr = String::from_utf8_lossy(&err.and_then(|t| t.join().ok()).unwrap_or_default())
        .trim()
        .to_owned();
    Ok(Output {
        code: status.code(),
        stdout,
        stderr,
    })
}

impl ExternalSecrets {
    #[must_use]
    /// The platform's real store: `secret-tool` on Linux, `security` on macOS.
    pub fn system() -> Self {
        if cfg!(target_os = "macos") {
            let program = Path::new("/usr/bin/security");
            Self::with_program(
                Backend::MacKeychain,
                if is_executable(program) {
                    Ok(program.to_path_buf())
                } else {
                    Err("The macOS `security` tool was not found at /usr/bin/security.".into())
                },
            )
        } else {
            let found = discover_in(
                "secret-tool",
                std::env::var_os("PATH").as_deref(),
                &standard_dirs(),
            );
            Self::with_program(
                Backend::SecretTool,
                found.ok_or_else(|| MISSING_SECRET_TOOL.to_string()),
            )
        }
    }

    #[must_use]
    pub(crate) fn with_program(backend: Backend, program: Result<PathBuf>) -> Self {
        Self {
            shared: Arc::new(Shared {
                backend,
                program,
                cache: Mutex::new(Cache::default()),
                refreshing: AtomicBool::new(false),
            }),
        }
    }
}

pub(crate) const MISSING_SECRET_TOOL: &str = "No secret store: `secret-tool` (libsecret) was not found. Install it (Nix: `nix profile add nixpkgs#libsecret`; Debian/Ubuntu: libsecret-tools; Fedora: libsecret; Arch: libsecret) and make sure a Secret Service such as GNOME Keyring or KWallet is running. OMP Manager never stores tokens or passwords in plaintext.";

impl Shared {
    fn program(&self) -> Result<&Path> {
        self.program.as_deref().map_err(Clone::clone)
    }

    fn unavailable(&self, detail: &str) -> String {
        match self.backend {
            Backend::SecretTool => format!("No secret service: {detail} Start and unlock GNOME Keyring / KWallet (a Secret Service on the session D-Bus) and retry. OMP Manager never stores tokens or passwords in plaintext."),
            Backend::MacKeychain => format!("The macOS Keychain is unavailable: {detail}"),
        }
    }

    /// A real round trip that does not create anything: an absent item answers exit 1 with no
    /// diagnostic, anything else on stderr means the service cannot be reached.
    fn probe(&self) -> Result<()> {
        let program = self.program()?;
        match self.backend {
            Backend::SecretTool => {
                let out = run(
                    program,
                    &["lookup", "service", SERVICE, "target", "omp-manager-probe"],
                    None,
                    PROBE_TIMEOUT,
                )?;
                match out.code {
                    Some(0) | Some(1) if out.stderr.is_empty() => Ok(()),
                    _ => Err(self.unavailable(&if out.stderr.is_empty() {
                        format!("secret-tool exited with {:?}.", out.code)
                    } else {
                        out.stderr
                    })),
                }
            }
            Backend::MacKeychain => {
                let out = run(program, &["default-keychain"], None, PROBE_TIMEOUT)?;
                if out.code == Some(0) {
                    Ok(())
                } else {
                    Err(self.unavailable(&out.stderr))
                }
            }
        }
    }

    fn verify_cached(&self) -> Result<()> {
        let result = self.probe();
        self.cache.lock().checked = Some((Instant::now(), result.clone()));
        result
    }
}

impl SecretStore for ExternalSecrets {
    fn status(&self) -> CredentialStoreStatus {
        let name = self.shared.backend.display().to_string();
        let cached = self.shared.cache.lock().checked.clone();
        let stale = cached
            .as_ref()
            .map(|(at, _)| at.elapsed() >= STATUS_TTL)
            .unwrap_or(true);
        if let Err(message) = &self.shared.program {
            // Nothing to probe: the answer is already final and free.
            return CredentialStoreStatus {
                name,
                available: false,
                message: Some(message.clone()),
            };
        }
        if stale && !self.shared.refreshing.swap(true, Ordering::AcqRel) {
            let shared = self.shared.clone();
            let spawned = std::thread::Builder::new()
                .name("code-quality-secret-probe".into())
                .spawn(move || {
                    let _ = shared.verify_cached();
                    shared.refreshing.store(false, Ordering::Release);
                });
            if spawned.is_err() {
                self.shared.refreshing.store(false, Ordering::Release);
            }
        }
        match cached {
            // Until the first background check lands the store is presumed usable; `verify()` is
            // authoritative and runs before anything is stored.
            None => CredentialStoreStatus {
                name,
                available: true,
                message: None,
            },
            Some((_, Ok(()))) => CredentialStoreStatus {
                name,
                available: true,
                message: None,
            },
            Some((_, Err(message))) => CredentialStoreStatus {
                name,
                available: false,
                message: Some(message),
            },
        }
    }

    fn verify(&self) -> Result<()> {
        self.shared.verify_cached()
    }

    fn write(&self, target: &str, secret: &str) -> Result<()> {
        let program = self.shared.program()?;
        if secret.is_empty() || secret.contains(['\0', '\n', '\r']) {
            return Err("A credential must be a single non-empty line of text.".into());
        }
        match self.shared.backend {
            Backend::SecretTool => {
                let label = target_label(target);
                let out = run(
                    program,
                    &[
                        "store",
                        &format!("--label={label}"),
                        "service",
                        SERVICE,
                        "target",
                        target,
                    ],
                    Some(secret.as_bytes()),
                    OPERATION_TIMEOUT,
                )?;
                if out.code == Some(0) {
                    return Ok(());
                }
                Err(self.shared.unavailable(&format!(
                    "could not save the credential ({}).",
                    if out.stderr.is_empty() {
                        format!("exit {:?}", out.code)
                    } else {
                        out.stderr
                    }
                )))
            }
            Backend::MacKeychain => {
                let command = format!(
                    "add-generic-password -U -a omp-manager -l {} -s {} -w {}\n",
                    keychain_word(&target_label(target)),
                    keychain_word(target),
                    keychain_word(secret)
                );
                let out = run(
                    program,
                    &["-i"],
                    Some(command.as_bytes()),
                    OPERATION_TIMEOUT,
                )?;
                // `security -i` does not reliably turn a failed command into a failing exit status:
                // trust only a read-back.
                match self.read(target)? {
                    Some(stored) if stored == secret => Ok(()),
                    _ => Err(self.shared.unavailable(&format!(
                        "could not save the credential ({}).",
                        if out.stderr.is_empty() {
                            "read-back mismatch".to_string()
                        } else {
                            out.stderr
                        }
                    ))),
                }
            }
        }
    }

    fn read(&self, target: &str) -> Result<Option<String>> {
        let program = self.shared.program()?;
        match self.shared.backend {
            Backend::SecretTool => {
                let out = run(
                    program,
                    &["lookup", "service", SERVICE, "target", target],
                    None,
                    OPERATION_TIMEOUT,
                )?;
                match out.code {
                    Some(0) => {
                        let text = String::from_utf8(out.stdout)
                            .map_err(|_| "The stored credential is not valid text.".to_string())?;
                        Ok(Some(text).filter(|t| !t.is_empty()))
                    }
                    Some(1) if out.stderr.is_empty() => Ok(None),
                    _ => Err(self.shared.unavailable(&format!(
                        "could not read the credential ({}).",
                        if out.stderr.is_empty() {
                            format!("exit {:?}", out.code)
                        } else {
                            out.stderr
                        }
                    ))),
                }
            }
            Backend::MacKeychain => {
                let out = run(
                    program,
                    &[
                        "find-generic-password",
                        "-a",
                        "omp-manager",
                        "-s",
                        target,
                        "-w",
                    ],
                    None,
                    OPERATION_TIMEOUT,
                )?;
                match out.code {
                    Some(0) => {
                        let text = String::from_utf8(out.stdout)
                            .map_err(|_| "The stored credential is not valid text.".to_string())?;
                        Ok(Some(text.trim_end_matches(['\n', '\r']).to_owned())
                            .filter(|t| !t.is_empty()))
                    }
                    // errSecItemNotFound
                    Some(44) => Ok(None),
                    _ => Err(self
                        .shared
                        .unavailable(&format!("could not read the credential ({}).", out.stderr))),
                }
            }
        }
    }

    fn delete(&self, target: &str) -> Result<()> {
        let program = self.shared.program()?;
        match self.shared.backend {
            Backend::SecretTool => {
                let out = run(
                    program,
                    &["clear", "service", SERVICE, "target", target],
                    None,
                    OPERATION_TIMEOUT,
                )?;
                match out.code {
                    Some(0) => Ok(()),
                    Some(1) if out.stderr.is_empty() => Ok(()), // nothing to delete
                    _ => Err(self.shared.unavailable(&format!(
                        "could not delete the credential ({}).",
                        if out.stderr.is_empty() {
                            format!("exit {:?}", out.code)
                        } else {
                            out.stderr
                        }
                    ))),
                }
            }
            Backend::MacKeychain => {
                let out = run(
                    program,
                    &["delete-generic-password", "-a", "omp-manager", "-s", target],
                    None,
                    OPERATION_TIMEOUT,
                )?;
                match out.code {
                    Some(0) | Some(44) => Ok(()),
                    _ => Err(self.shared.unavailable(&format!(
                        "could not delete the credential ({}).",
                        out.stderr
                    ))),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    use uuid::Uuid;

    struct Fake {
        dir: PathBuf,
        tool: PathBuf,
    }

    impl Fake {
        /// A stand-in `secret-tool` that keeps items in `<dir>/items`, logs its argv to
        /// `<dir>/argv.log` and its environment to `<dir>/env.log`, and answers like libsecret
        /// (lookup miss: exit 1, no output). `mode` selects failure behaviour.
        fn new(mode: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("omp-fake-secret-tool-{}", Uuid::new_v4().simple()));
            std::fs::create_dir_all(dir.join("items")).unwrap();
            let tool = dir.join("secret-tool");
            let script = format!(
                r#"#!/bin/sh
D='{dir}'
printf '%s\n' "$*" >> "$D/argv.log"
env > "$D/env.log"
case '{mode}' in
  nobus) echo 'secret-tool: Cannot autolaunch D-Bus without X11 $DISPLAY' >&2; exit 1;;
  hang) sleep 30; exit 0;;
esac
action="$1"; shift
# attributes: service <s> target <t>   (store has a leading --label=...)
case "$action" in store) shift;; esac
target="$4"
key=$(printf '%s' "$target" | od -An -tx1 | tr -d ' \n')
case "$action" in
  store) cat > "$D/items/$key";;
  lookup) if [ -f "$D/items/$key" ]; then cat "$D/items/$key"; else exit 1; fi;;
  clear) if [ -f "$D/items/$key" ]; then rm "$D/items/$key"; else exit 1; fi;;
  *) echo "unknown action $action" >&2; exit 2;;
esac
"#,
                dir = dir.display(),
                mode = mode
            );
            std::fs::write(&tool, script).unwrap();
            std::fs::set_permissions(&tool, Permissions::from_mode(0o755)).unwrap();
            Fake { dir, tool }
        }
        fn store(&self) -> ExternalSecrets {
            ExternalSecrets::with_program(Backend::SecretTool, Ok(self.tool.clone()))
        }
        fn log(&self, name: &str) -> String {
            std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn secret_travels_on_stdin_never_argv_or_environment() {
        let fake = Fake::new("ok");
        let store = fake.store();
        let secret = "squ_0123456789abcdef-secret";
        store
            .write("OMPManager.CodeQuality/aa/bb/token", secret)
            .unwrap();
        // The tool received the secret on stdin and stored it.
        let stored: Vec<_> = std::fs::read_dir(fake.dir.join("items")).unwrap().collect();
        assert_eq!(stored.len(), 1);
        assert_eq!(
            std::fs::read_to_string(stored[0].as_ref().unwrap().path()).unwrap(),
            secret
        );
        // argv carried only the verb, label and non-secret attributes.
        let argv = fake.log("argv.log");
        assert!(argv.contains("store --label=OMP Manager code quality (token) service omp-manager.code-quality target OMPManager.CodeQuality/aa/bb/token"), "{argv}");
        assert!(!argv.contains(secret) && !fake.log("env.log").contains(secret));
    }

    #[test]
    fn round_trip_replace_missing_and_delete() {
        let fake = Fake::new("ok");
        let store = fake.store();
        assert_eq!(store.read("t/one").unwrap(), None);
        store.write("t/one", "first").unwrap();
        assert_eq!(store.read("t/one").unwrap().as_deref(), Some("first"));
        store.write("t/one", "second").unwrap();
        assert_eq!(store.read("t/one").unwrap().as_deref(), Some("second"));
        assert_eq!(store.read("t/two").unwrap(), None, "targets do not collide");
        store.delete("t/one").unwrap();
        assert_eq!(store.read("t/one").unwrap(), None);
        store.delete("t/one").unwrap(); // deleting a missing credential is not an error
    }

    #[test]
    fn secrets_are_read_back_exactly_without_a_trailing_newline() {
        let fake = Fake::new("ok");
        let store = fake.store();
        store.write("t/exact", "Omp-abc-Aa1!").unwrap();
        assert_eq!(
            store.read("t/exact").unwrap().as_deref(),
            Some("Omp-abc-Aa1!")
        );
    }

    #[test]
    fn multiline_and_empty_secrets_are_refused_before_the_tool_runs() {
        let fake = Fake::new("ok");
        let store = fake.store();
        for bad in ["", "a\nb", "a\r", "a\0b"] {
            assert!(store.write("t/bad", bad).is_err(), "{bad:?}");
        }
        assert!(
            fake.log("argv.log").is_empty(),
            "the tool must not be started for invalid input"
        );
    }

    #[test]
    fn child_environment_is_an_allowlist() {
        let fake = Fake::new("ok");
        let store = fake.store();
        store.read("t/env").unwrap();
        let env = fake.log("env.log");
        // Cargo sets CARGO_MANIFEST_DIR for the test process; it is not on the allowlist.
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        assert!(!env.contains("CARGO_MANIFEST_DIR"), "{env}");
    }

    #[test]
    fn missing_session_bus_is_reported_not_swallowed() {
        let fake = Fake::new("nobus");
        let store = fake.store();
        let error = store.read("t/x").unwrap_err();
        assert!(
            error.contains("No secret service") && error.contains("Cannot autolaunch D-Bus"),
            "{error}"
        );
        assert!(error.contains("never stores tokens or passwords in plaintext"));
        assert!(store.write("t/x", "value").is_err());
        assert!(store.verify().is_err());
        let status = store.status();
        assert!(
            !status.available
                && status
                    .message
                    .as_deref()
                    .unwrap()
                    .contains("No secret service"),
            "{status:?}"
        );
        assert_eq!(status.name, "Secret Service (secret-tool)");
    }

    #[test]
    fn missing_secret_tool_is_an_explicit_state_and_nothing_is_written() {
        let store =
            ExternalSecrets::with_program(Backend::SecretTool, Err(MISSING_SECRET_TOOL.into()));
        let status = store.status();
        assert!(!status.available);
        assert!(status
            .message
            .as_deref()
            .unwrap()
            .contains("nix profile add nixpkgs#libsecret"));
        assert!(store.verify().unwrap_err().contains("secret-tool"));
        assert!(store
            .write("t/x", "value")
            .unwrap_err()
            .contains("secret-tool"));
        assert!(store.read("t/x").is_err());
        assert!(store.delete("t/x").is_err());
    }

    #[test]
    fn healthy_store_verifies_and_reports_available() {
        let fake = Fake::new("ok");
        let store = fake.store();
        store.verify().unwrap();
        let status = store.status();
        assert!(status.available && status.message.is_none());
    }

    #[test]
    fn a_hung_service_times_out_instead_of_blocking_forever() {
        let fake = Fake::new("hang");
        let error = run(
            &fake.tool,
            &["lookup", "service", "x", "target", "y"],
            None,
            Duration::from_millis(300),
        )
        .err()
        .unwrap();
        assert!(error.contains("did not answer"), "{error}");
    }

    #[test]
    fn discovery_prefers_path_then_standard_directories_and_ignores_non_executables() {
        let base = std::env::temp_dir().join(format!("omp-discover-{}", Uuid::new_v4().simple()));
        let (a, b, c) = (base.join("a"), base.join("b"), base.join("c"));
        for dir in [&a, &b, &c] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(a.join("secret-tool"), "#!/bin/sh\n").unwrap(); // not executable
        std::fs::write(b.join("secret-tool"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(b.join("secret-tool"), Permissions::from_mode(0o755)).unwrap();
        std::fs::write(c.join("secret-tool"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(c.join("secret-tool"), Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([&a, &b]).unwrap();
        assert_eq!(
            discover_in("secret-tool", Some(&path), std::slice::from_ref(&c)),
            Some(b.join("secret-tool"))
        );
        assert_eq!(
            discover_in(
                "secret-tool",
                Some(&OsString::from(a.to_str().unwrap())),
                std::slice::from_ref(&c)
            ),
            Some(c.join("secret-tool"))
        );
        assert_eq!(discover_in("secret-tool", None, &[]), None);
        // A relative PATH entry (including the empty "current directory" entry) is never trusted.
        assert_eq!(
            discover_in("secret-tool", Some(&OsString::from("")), &[]),
            None
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn keychain_commands_quote_every_special_character() {
        assert_eq!(keychain_word("plain"), "\"plain\"");
        assert_eq!(keychain_word(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(keychain_word("with space"), "\"with space\"");
    }
}
