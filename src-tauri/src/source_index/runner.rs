//! The trusted local parser: embedded worker scripts, interpreter discovery, capability probing and the
//! supervised extraction process. Only app-owned scripts run; the repository supplies data, never code, module
//! names or interpreter options. This is a local parser, not an OS sandbox.
use super::model::{ProfileCapability, Result, RuntimeCapability, SourceIndexCapabilities};
use crate::code_quality::runtime::{self, RunControl};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

include!(concat!(env!("OUT_DIR"), "/source_index_scripts.rs"));

pub(crate) const PYTHON_OVERRIDE: &str = "OMP_SYSTEM_OVERVIEW_PYTHON";
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct ExtractRun<'run> {
    pub manifest: &'run Path,
    pub output: &'run Path,
    /// App-owned scratch directory; the worker writes `progress.json` beside the output.
    pub work_dir: &'run Path,
    pub timeout: Duration,
    pub cancel: Arc<AtomicBool>,
}

#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParserOutcome {
    Finished,
    Cancelled,
    TimedOut,
    Failed(String),
}

/// Seam between the job pipeline and the process that produces facts (replaced by an in-process fake in tests).
pub(crate) trait Parser: Send + Sync {
    #[must_use]
    fn probe(&self) -> SourceIndexCapabilities;
    fn extract(&self, run: &ExtractRun) -> ParserOutcome;
    /// Content hash of the worker scripts that produce the facts.
    #[must_use]
    fn scripts_hash(&self) -> String;
}

fn scripts_hash() -> String {
    let mut hash = Sha256::new();
    for (name, body) in SCRIPTS {
        hash.update((name.len() as u64).to_be_bytes());
        hash.update(name.as_bytes());
        hash.update((body.len() as u64).to_be_bytes());
        hash.update(body.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

/// Writes the embedded scripts into a content-addressed directory below `root` (rewriting any file that was
/// altered on disk) and returns it.
pub(crate) fn materialize_scripts(root: &Path) -> Result<PathBuf> {
    if !SCRIPTS.iter().any(|(name, _)| *name == "worker.py") {
        return Err("The source index worker script is not part of this build".into());
    }
    let directory = root.join("worker").join(&scripts_hash()[..16]);
    fs::create_dir_all(&directory)
        .map_err(|e| format!("Cannot create the source index worker directory: {e}"))?;
    for (name, body) in SCRIPTS {
        let file = directory.join(name);
        if fs::read_to_string(&file).ok().as_deref() != Some(*body) {
            fs::write(&file, body)
                .map_err(|e| format!("Cannot write worker script {name}: {e}"))?;
        }
    }
    Ok(directory)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Interpreter {
    pub path: PathBuf,
    pub source: &'static str,
}

/// Explicit override first, then the Graphify environment on Windows or `python3` elsewhere. Never installs.
pub(crate) fn discover_interpreter(
    override_value: Option<OsString>,
    local_app_data: Option<OsString>,
) -> std::result::Result<Interpreter, (&'static str, String)> {
    if let Some(value) = override_value.filter(|v| !v.is_empty()) {
        let path = PathBuf::from(value);
        return if path.is_file() {
            Ok(Interpreter {
                path,
                source: "environment",
            })
        } else {
            Err((
                "environment",
                format!(
                    "{PYTHON_OVERRIDE} does not point to a file: {}",
                    path.display()
                ),
            ))
        };
    }
    if cfg!(windows) {
        let Some(base) = local_app_data else {
            return Err((
                "graphify",
                "LOCALAPPDATA is not set, so the Graphify Python environment cannot be located"
                    .into(),
            ));
        };
        let path = PathBuf::from(base)
            .join("Graphify")
            .join("venv")
            .join("Scripts")
            .join("python.exe");
        return if path.is_file() {
            Ok(Interpreter {
                path,
                source: "graphify",
            })
        } else {
            Err((
                "graphify",
                format!(
                    "The Graphify Python environment was not found at {}",
                    path.display()
                ),
            ))
        };
    }
    match runtime::discover_program("python3", &[]) {
        Some(path) => Ok(Interpreter {
            path,
            source: "path",
        }),
        None => Err(("path", "python3 was not found on PATH".into())),
    }
}

#[must_use]
pub(crate) fn unavailable(source: &str, error: String) -> SourceIndexCapabilities {
    SourceIndexCapabilities {
        available: false, probed: true,
        runtime: RuntimeCapability { source: source.into(), error: Some(error.clone()), ..Default::default() },
        profiles: Vec::new(),
        hint: Some(format!("{error}. Set {PYTHON_OVERRIDE} to a Python interpreter that has tree-sitter 0.25.2 and the language grammar packages (for example the Graphify environment). The app never installs or updates packages; the file inventory and structure remain available.")),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProbeRuntime {
    python: Option<String>,
    #[serde(default)]
    tree_sitter: Option<ProbeTreeSitter>,
}
/// `worker.py capabilities` reports `{version, languageVersion, minCompatibleLanguageVersion, error}`; a bare version string is also accepted.
#[derive(Deserialize)]
#[serde(untagged)]
enum ProbeTreeSitter {
    Version(String),
    Detail {
        version: Option<String>,
        error: Option<String>,
    },
}
impl ProbeTreeSitter {
    fn version(self) -> Option<String> {
        match self {
            Self::Version(version) => Some(version),
            Self::Detail {
                version,
                error: None,
            } => version,
            Self::Detail { .. } => None,
        }
    }
}
#[derive(Deserialize)]
struct ProbeProfile {
    language: String,
    version: Option<String>,
    available: bool,
    error: Option<String>,
}
#[derive(Deserialize)]
struct Probe {
    runtime: ProbeRuntime,
    profiles: Vec<ProbeProfile>,
}

/// Parses `worker.py capabilities` output; anything malformed is a visible error, never an empty success.
#[must_use]
pub(crate) fn parse_probe(interpreter: &Interpreter, stdout: &[u8]) -> SourceIndexCapabilities {
    let probe: Probe = match serde_json::from_slice(stdout) {
        Ok(probe) => probe,
        Err(e) => {
            return unavailable(
                interpreter.source,
                format!("The parser capability report is not valid: {e}"),
            )
        }
    };
    let profiles: Vec<ProfileCapability> = probe
        .profiles
        .into_iter()
        .map(|p| ProfileCapability {
            language: p.language,
            version: p.version,
            available: p.available,
            error: p.error,
        })
        .collect();
    let usable = profiles.iter().filter(|p| p.available).count();
    let runtime = RuntimeCapability {
        source: interpreter.source.into(),
        python: Some(interpreter.path.to_string_lossy().into_owned()),
        version: probe.runtime.python,
        tree_sitter: probe.runtime.tree_sitter.and_then(ProbeTreeSitter::version),
        error: None,
    };
    if usable == 0 {
        let mut result = unavailable(
            interpreter.source,
            "No Tree-sitter grammar is usable in the selected Python environment".into(),
        );
        result.runtime = RuntimeCapability {
            error: Some(
                "No Tree-sitter grammar is usable in the selected Python environment".into(),
            ),
            ..runtime
        };
        result.profiles = profiles;
        return result;
    }
    SourceIndexCapabilities {
        available: true,
        probed: true,
        runtime,
        profiles,
        hint: None,
    }
}

/// Production parser: runs `python -I -B <materialized worker.py>` from an app-owned directory.
pub(crate) struct PythonParser {
    scripts: PathBuf,
}

impl PythonParser {
    pub fn try_new(config_root: &Path) -> Result<Self> {
        Ok(Self {
            scripts: materialize_scripts(config_root)?,
        })
    }

    fn command(&self, interpreter: &Interpreter, cwd: &Path) -> Command {
        let mut command = Command::new(&interpreter.path);
        // -I: isolated mode (no PYTHON* variables, no user site, no script-directory or cwd imports).
        command
            .arg("-I")
            .arg("-B")
            .arg(self.scripts.join("worker.py"))
            .current_dir(cwd);
        command
    }

    fn interpreter(&self) -> std::result::Result<Interpreter, (&'static str, String)> {
        discover_interpreter(
            std::env::var_os(PYTHON_OVERRIDE),
            std::env::var_os("LOCALAPPDATA"),
        )
    }
}

impl Parser for PythonParser {
    fn probe(&self) -> SourceIndexCapabilities {
        let interpreter = match self.interpreter() {
            Ok(i) => i,
            Err((source, error)) => return unavailable(source, error),
        };
        let mut command = self.command(&interpreter, &self.scripts);
        command.arg("capabilities");
        let control = RunControl::silent();
        match runtime::run_bounded(command, None, PROBE_TIMEOUT, &control) {
            Err(e) => unavailable(
                interpreter.source,
                format!("The parser runtime could not be started: {e}"),
            ),
            Ok(out) if out.timed_out => unavailable(
                interpreter.source,
                "The parser capability probe timed out".into(),
            ),
            Ok(out) if !out.status.success() => unavailable(
                interpreter.source,
                format!(
                    "The parser capability probe failed: {}",
                    out.diagnostic().chars().take(1500).collect::<String>()
                ),
            ),
            Ok(out) => parse_probe(&interpreter, &out.stdout),
        }
    }

    fn extract(&self, run: &ExtractRun) -> ParserOutcome {
        let interpreter = match self.interpreter() {
            Ok(i) => i,
            Err((_, error)) => return ParserOutcome::Failed(error),
        };
        let mut command = self.command(&interpreter, run.work_dir);
        command
            .arg("extract")
            .arg("--manifest")
            .arg(run.manifest)
            .arg("--output")
            .arg(run.output);
        let control = RunControl {
            cancel: run.cancel.clone(),
            log: Arc::new(|_| {}),
        };
        match runtime::run_bounded(command, None, run.timeout, &control) {
            Err(e) => ParserOutcome::Failed(e),
            Ok(out) if out.cancelled => ParserOutcome::Cancelled,
            Ok(out) if out.timed_out => ParserOutcome::TimedOut,
            Ok(out) if !out.status.success() => ParserOutcome::Failed(format!(
                "The parser worker failed: {}",
                out.diagnostic().chars().take(2000).collect::<String>()
            )),
            Ok(_) => ParserOutcome::Finished,
        }
    }

    fn scripts_hash(&self) -> String {
        scripts_hash()
    }
}

/// Stand-in when the worker scripts could not even be written; every job then reports the reason.
pub(crate) struct UnavailableParser(pub String);

impl Parser for UnavailableParser {
    fn probe(&self) -> SourceIndexCapabilities {
        unavailable("none", self.0.clone())
    }
    fn extract(&self, _run: &ExtractRun) -> ParserOutcome {
        ParserOutcome::Failed(self.0.clone())
    }
    fn scripts_hash(&self) -> String {
        scripts_hash()
    }
}
