//! Windows Job Object containment.
//!
//! `contain` adopts an already running process into a kill-on-close job. `ContainedChild` launches a process
//! suspended, assigns it to its job before it executes a single instruction, and owns its retirement: termination is
//! `TerminateJobObject` while the job handle stays open, then active-process accounting until the whole tree is gone.
//! Win32 job, handle-inheritance and suspended-launch APIs have no equivalent in the standard library.
use parking_lot::{Condvar, Mutex};
use std::ffi::{c_void, OsStr, OsString};
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    FILETIME, HANDLE, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, GetProcessTimes,
    InitializeProcThreadAttributeList, OpenProcess, ResumeThread, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

/// Upper bound for job-emptiness accounting after `TerminateJobObject`.
pub const RETIRE_BOUND: Duration = Duration::from_secs(5);
const MAX_COMMAND_LINE: usize = 32_000;

fn last_error(context: &str) -> String {
    format!("{context}: {}", std::io::Error::last_os_error())
}

fn new_job(subject: &str) -> Result<OwnedHandle, String> {
    // SAFETY: Win32 returns a new owned handle; the zeroed limits POD and its byte count match the queried class.
    unsafe {
        let raw = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if raw.is_null() {
            return Err(last_error(&format!("Create {subject} job")));
        }
        let job = OwnedHandle::from_raw_handle(raw);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            raw,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            std::mem::size_of_val(&limits) as u32,
        ) == 0
        {
            return Err(last_error(&format!("Set {subject} job limits")));
        }
        Ok(job)
    }
}

/// Put `pid` in a new kill-on-close job. Keep the handle alive for as long as the process tree must live;
/// `subject` names the owner in error messages.
pub fn contain(pid: u32, subject: &str) -> Result<OwnedHandle, String> {
    let job = new_job(subject)?;
    // SAFETY: a successful OpenProcess transfers a valid handle, owned until assignment completes; the job stays live.
    unsafe {
        let raw_process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if raw_process.is_null() {
            return Err(last_error(&format!("Open {subject} process")));
        }
        let process = OwnedHandle::from_raw_handle(raw_process);
        if AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) == 0 {
            return Err(last_error(&format!("Assign {subject} job")));
        }
    }
    Ok(job)
}

/// Processes currently alive in `job` (root and descendants).
pub fn active_processes(job: &OwnedHandle) -> Result<u32, String> {
    // SAFETY: the borrowed job stays open; the zeroed accounting POD is writable for the supplied exact size.
    unsafe {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
        if QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of_val(&info) as u32,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(last_error("Query job accounting"));
        }
        Ok(info.ActiveProcesses)
    }
}

/// Terminate everything in `job`, then wait (at most `bound`) until its accounting reports zero active processes.
/// The job handle is borrowed: the caller keeps ownership, so a failure leaves kill-on-close protection intact.
pub fn terminate_job(job: &OwnedHandle, subject: &str, bound: Duration) -> Result<(), String> {
    // SAFETY: the borrowed job handle remains live throughout termination and subsequent accounting.
    unsafe {
        if TerminateJobObject(job.as_raw_handle(), 1) == 0 {
            return Err(last_error(&format!("Terminate {subject} job")));
        }
    }
    let deadline = Instant::now() + bound;
    loop {
        let active = active_processes(job)?;
        if active == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{subject} still has {active} active process(es) {} seconds after termination",
                bound.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn filetime_u64(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

fn process_creation_time_of(process: &OwnedHandle) -> Result<u64, String> {
    // SAFETY: the owned process handle stays live; each FILETIME out-pointer refers to initialized writable storage.
    unsafe {
        let (mut created, mut exited, mut kernel, mut user): (
            FILETIME,
            FILETIME,
            FILETIME,
            FILETIME,
        ) = (
            std::mem::zeroed(),
            std::mem::zeroed(),
            std::mem::zeroed(),
            std::mem::zeroed(),
        );
        if GetProcessTimes(
            process.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        ) == 0
        {
            return Err(last_error("Read process creation time"));
        }
        Ok(filetime_u64(created))
    }
}

/// FILETIME creation identity of a currently running process; `None` when it cannot be inspected or has exited.
/// PID alone is never an identity: callers compare this value with the one they captured at launch.
pub fn process_creation_time(pid: u32) -> Option<u64> {
    // SAFETY: a non-null OpenProcess handle is owned immediately and used only while its owner is live.
    unsafe {
        let raw = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
        if raw.is_null() {
            return None;
        }
        let process = OwnedHandle::from_raw_handle(raw);
        if WaitForSingleObject(process.as_raw_handle(), 0) != WAIT_TIMEOUT {
            return None;
        }
        process_creation_time_of(&process).ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StdioMode {
    Null,
    Capture,
}

pub struct ContainedSpec {
    /// Names the owner in error messages.
    pub subject: String,
    /// Absolute path of the executable (`lpApplicationName`; no PATH search, no shell).
    pub program: PathBuf,
    /// `argv[1..]`, literal; quoted for the Windows command line here.
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    /// The child inherits this process's environment, minus `env_remove`, plus `env_set` (names are case-insensitive).
    pub env_set: Vec<(OsString, OsString)>,
    pub env_remove: Vec<OsString>,
    pub stdout: StdioMode,
    pub stderr: StdioMode,
    /// Bytes retained per captured stream (the most recent ones).
    pub capture_limit: usize,
}

struct CaptureState {
    data: Vec<u8>,
    eof: bool,
}

/// Bounded tail of a child's output stream, readable while the child runs.
pub struct Capture {
    state: Mutex<CaptureState>,
    changed: Condvar,
    limit: usize,
}

impl Capture {
    fn new(limit: usize, eof: bool) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CaptureState {
                data: Vec::new(),
                eof,
            }),
            changed: Condvar::new(),
            limit: limit.max(1),
        })
    }
    fn push(&self, bytes: &[u8]) {
        let mut state = self.state.lock();
        state.data.extend_from_slice(bytes);
        if state.data.len() > self.limit {
            let excess = state.data.len() - self.limit;
            state.data.drain(..excess);
        }
        self.changed.notify_all();
    }
    fn finish(&self) {
        self.state.lock().eof = true;
        self.changed.notify_all();
    }
    /// The retained output so far (lossy UTF-8).
    #[must_use]
    pub fn tail(&self) -> String {
        String::from_utf8_lossy(&self.state.lock().data).into_owned()
    }
    /// Evaluates `f` on the retained text now and after every new chunk until it returns `Some`, the stream ends or
    /// `timeout` passes.
    pub fn wait_match<T>(&self, timeout: Duration, f: impl FnMut(&str) -> Option<T>) -> Option<T> {
        self.wait_match_until(timeout, &|| false, f)
    }
    /// Like `wait_match`, additionally giving up when `stop` turns true (polled about every 50 ms).
    pub fn wait_match_until<T>(
        &self,
        timeout: Duration,
        stop: &dyn Fn() -> bool,
        mut f: impl FnMut(&str) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock();
        loop {
            if let Some(found) = f(&String::from_utf8_lossy(&state.data)) {
                return Some(found);
            }
            if state.eof || stop() {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            self.changed
                .wait_for(&mut state, (deadline - now).min(Duration::from_millis(50)));
        }
    }
}

fn drain(mut source: File, capture: Arc<Capture>) {
    use std::io::Read;
    let mut buffer = [0u8; 4096];
    loop {
        match source.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => capture.push(&buffer[..count]),
        }
    }
    capture.finish();
}

struct Inner {
    process: Option<OwnedHandle>,
    job: Option<OwnedHandle>,
    exit_code: Option<u32>,
}

/// A process launched suspended into its own kill-on-close job. All methods take `&self`; the value is `Send + Sync`.
/// Dropping it while still alive closes the job handle, which kills the tree; prefer an explicit `terminate`.
pub struct ContainedChild {
    subject: String,
    pid: u32,
    creation: u64,
    inner: Mutex<Inner>,
    stdout: Arc<Capture>,
    stderr: Arc<Capture>,
}

struct AttributeList {
    storage: Vec<u64>,
}
impl AttributeList {
    fn with_handles(handles: &[HANDLE]) -> Result<Self, String> {
        // SAFETY: Win32 reports the required size; u64 storage is suitably aligned, initialized before update, and
        // the handle slice remains live until CreateProcessW consumes the attribute list.
        unsafe {
            let mut size = 0usize;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
            if size == 0 {
                return Err(last_error("Size process attribute list"));
            }
            let mut storage = vec![0u64; size.div_ceil(8)];
            if InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 1, 0, &mut size) == 0
            {
                return Err(last_error("Initialize process attribute list"));
            }
            let mut list = Self { storage };
            if UpdateProcThreadAttribute(
                list.pointer(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                std::mem::size_of_val(handles),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) == 0
            {
                return Err(last_error("Restrict inherited handles"));
            }
            Ok(list)
        }
    }
    fn pointer(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
}
impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: storage contains the successfully initialized list and is freed only after this destructor returns.
        unsafe { DeleteProcThreadAttributeList(self.pointer()) };
    }
}

fn set_inheritable(handle: &OwnedHandle, inherit: bool) -> Result<(), String> {
    let flags = if inherit { HANDLE_FLAG_INHERIT } else { 0 };
    // SAFETY: callers pass live owned stdio handles; this only changes their inheritance flag.
    if unsafe {
        windows_sys::Win32::Foundation::SetHandleInformation(
            handle.as_raw_handle(),
            HANDLE_FLAG_INHERIT,
            flags,
        )
    } == 0
    {
        return Err(last_error("Set handle inheritance"));
    }
    Ok(())
}

/// Anonymous pipe the child writes and the parent reads; returns `(parent_read, child_write)`.
fn output_pipe() -> Result<(OwnedHandle, OwnedHandle), String> {
    // SAFETY: the initialized security attributes and output handle slots live through CreatePipe; successful
    // handles are each adopted exactly once before any fallible work.
    unsafe {
        let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        if CreatePipe(&mut read, &mut write, &attributes, 0) == 0 {
            return Err(last_error("Create pipe"));
        }
        let (read, write) = (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        );
        set_inheritable(&read, /* inherit= */ false)?;
        Ok((read, write))
    }
}

fn nul(write: bool) -> Result<OwnedHandle, String> {
    let file = std::fs::OpenOptions::new()
        .read(!write)
        .write(write)
        .open("NUL")
        .map_err(|e| format!("Open NUL: {e}"))?;
    let handle = OwnedHandle::from(file);
    set_inheritable(&handle, /* inherit= */ true)?;
    Ok(handle)
}

fn wide(text: &OsStr, what: &str) -> Result<Vec<u16>, String> {
    let mut units: Vec<u16> = text.encode_wide().collect();
    if units.contains(&0) {
        return Err(format!("{what} contains a NUL character"));
    }
    units.push(0);
    Ok(units)
}

/// Appends one argument using the MSVCRT quoting rules every Windows runtime (Node, Chrome) parses.
fn append_argument(command: &mut Vec<u16>, argument: &OsStr) -> Result<(), String> {
    let units: Vec<u16> = argument.encode_wide().collect();
    if units.contains(&0) {
        return Err("argument contains a NUL character".into());
    }
    let quote = units.is_empty()
        || units
            .iter()
            .any(|&c| c == u16::from(b' ') || c == u16::from(b'\t') || c == u16::from(b'"'));
    if quote {
        command.push(u16::from(b'"'));
    }
    let mut backslashes = 0usize;
    for &unit in &units {
        if unit == u16::from(b'\\') {
            backslashes += 1;
        } else {
            if unit == u16::from(b'"') {
                command.extend(std::iter::repeat(u16::from(b'\\')).take(backslashes + 1));
            }
            backslashes = 0;
        }
        command.push(unit);
    }
    if quote {
        command.extend(std::iter::repeat(u16::from(b'\\')).take(backslashes));
        command.push(u16::from(b'"'));
    }
    Ok(())
}

fn command_line(program: &OsStr, args: &[OsString]) -> Result<Vec<u16>, String> {
    let mut line = Vec::new();
    append_argument(&mut line, program)?;
    for argument in args {
        line.push(u16::from(b' '));
        append_argument(&mut line, argument)?;
    }
    if line.len() > MAX_COMMAND_LINE {
        return Err("command line is too long".into());
    }
    line.push(0);
    Ok(line)
}

fn same_name(a: &OsStr, b: &OsStr) -> bool {
    a.to_string_lossy().to_uppercase() == b.to_string_lossy().to_uppercase()
}

fn environment_block(
    set: &[(OsString, OsString)],
    remove: &[OsString],
) -> Result<Vec<u16>, String> {
    for (name, value) in set {
        let text = name.to_string_lossy();
        if name.is_empty()
            || text.contains('=')
            || name.encode_wide().any(|c| c == 0)
            || value.encode_wide().any(|c| c == 0)
        {
            return Err("invalid environment variable".into());
        }
    }
    let mut variables: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(name, _)| {
            !remove.iter().any(|r| same_name(name, r))
                && !set.iter().any(|(s, _)| same_name(name, s))
        })
        .collect();
    variables.extend(set.iter().cloned());
    variables.sort_by_key(|(name, _)| {
        name.to_string_lossy()
            .to_uppercase()
            .encode_utf16()
            .collect::<Vec<u16>>()
    });
    let mut block = Vec::new();
    for (name, value) in variables {
        block.extend(name.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    Ok(block)
}

impl ContainedChild {
    /// `CreateProcessW` suspended, assign the kill-on-close job, then resume. Only the stdio handles of the spec are
    /// inherited (explicit handle list); stdin is always NUL. Any failure kills the not-yet-running process.
    pub fn spawn(spec: ContainedSpec) -> Result<Self, String> {
        let subject = spec.subject;
        if !spec.program.is_absolute() {
            return Err(format!("{subject} executable must be an absolute path"));
        }
        let application = wide(spec.program.as_os_str(), "executable path")?;
        let mut line = command_line(spec.program.as_os_str(), &spec.args)?;
        let environment = environment_block(&spec.env_set, &spec.env_remove)?;
        let directory = spec
            .cwd
            .as_ref()
            .map(|cwd| wide(cwd.as_os_str(), "working directory"))
            .transpose()?;

        let stdin = nul(/* write= */ false)?;
        let (stdout_parent, stdout_child) = match spec.stdout {
            StdioMode::Capture => {
                let (parent, child) = output_pipe()?;
                (Some(parent), child)
            }
            StdioMode::Null => (None, nul(/* write= */ true)?),
        };
        let (stderr_parent, stderr_child) = match spec.stderr {
            StdioMode::Capture => {
                let (parent, child) = output_pipe()?;
                (Some(parent), child)
            }
            StdioMode::Null => (None, nul(/* write= */ true)?),
        };
        let handles = [
            stdin.as_raw_handle(),
            stdout_child.as_raw_handle(),
            stderr_child.as_raw_handle(),
        ];
        let mut attributes = AttributeList::with_handles(&handles)?;
        let job = new_job(&subject)?;

        // SAFETY: these Win32 POD structs permit all-zero initialization; required fields are populated before use.
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = handles[0];
        startup.StartupInfo.hStdOutput = handles[1];
        startup.StartupInfo.hStdError = handles[2];
        startup.lpAttributeList = attributes.pointer();
        let mut information: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: UTF-16 strings and the environment block are terminated and remain live; line is writable,
        // attribute/stdin/out/err handles remain owned, and information is a full writable PROCESS_INFORMATION.
        let created = unsafe {
            CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_SUSPENDED
                    | EXTENDED_STARTUPINFO_PRESENT
                    | CREATE_UNICODE_ENVIRONMENT
                    | CREATE_NO_WINDOW,
                environment.as_ptr().cast(),
                directory.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
                &startup as *const STARTUPINFOEXW as *const STARTUPINFOW,
                &mut information,
            )
        };
        if created == 0 {
            return Err(last_error(&format!("Start {subject}")));
        }
        // SAFETY: successful CreateProcessW returns two distinct newly owned handles.
        let (process, thread) = unsafe {
            (
                OwnedHandle::from_raw_handle(information.hProcess),
                OwnedHandle::from_raw_handle(information.hThread),
            )
        };
        // The child owns its copies now; keeping ours would hold its pipes open after it exits.
        drop((stdin, stdout_child, stderr_child, attributes));

        let abort = |process: &OwnedHandle, message: String| -> String {
            // SAFETY: process is still owned and suspended, so termination cannot run uncontained child code.
            unsafe { TerminateProcess(process.as_raw_handle(), 1) };
            message
        };
        // SAFETY: both handles remain owned; the suspended process has not executed before job assignment.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) } == 0 {
            return Err(abort(
                &process,
                last_error(&format!("Assign {subject} job")),
            ));
        }
        let creation = match process_creation_time_of(&process) {
            Ok(creation) => creation,
            Err(error) => return Err(abort(&process, error)),
        };
        // SAFETY: thread is the live primary-thread handle returned by the successful suspended launch.
        if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
            return Err(abort(&process, last_error(&format!("Resume {subject}"))));
        }
        drop(thread);

        let capture = |parent: Option<OwnedHandle>, limit: usize| {
            let capture = Capture::new(limit, parent.is_none());
            if let Some(parent) = parent {
                let file = File::from(parent);
                let sink = capture.clone();
                std::thread::spawn(move || drain(file, sink));
            }
            capture
        };
        Ok(Self {
            pid: information.dwProcessId,
            creation,
            stdout: capture(stdout_parent, spec.capture_limit),
            stderr: capture(stderr_parent, spec.capture_limit),
            inner: Mutex::new(Inner {
                process: Some(process),
                job: Some(job),
                exit_code: None,
            }),
            subject,
        })
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }
    /// FILETIME ticks (100 ns since 1601-01-01 UTC) at which the root process was created.
    #[must_use]
    pub fn creation_time_100ns(&self) -> u64 {
        self.creation
    }
    #[must_use]
    pub fn stdout(&self) -> &Capture {
        &self.stdout
    }
    #[must_use]
    pub fn stderr(&self) -> &Capture {
        &self.stderr
    }

    /// Exit code once the root process has exited.
    pub fn try_wait(&self) -> Result<Option<u32>, String> {
        self.wait_slice(0)
    }

    pub fn wait_timeout(&self, timeout: Duration) -> Result<Option<u32>, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if let Some(code) =
                self.wait_slice(remaining.min(Duration::from_millis(50)).as_millis() as u32)?
            {
                return Ok(Some(code));
            }
            if remaining.is_zero() {
                return Ok(None);
            }
        }
    }

    fn wait_slice(&self, milliseconds: u32) -> Result<Option<u32>, String> {
        let mut inner = self.inner.lock();
        let Some(process) = inner.process.as_ref() else {
            return Ok(inner.exit_code);
        };
        let raw = process.as_raw_handle();
        // SAFETY: the inner lock keeps the owned process handle open throughout this wait.
        match unsafe { WaitForSingleObject(raw, milliseconds) } {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                // SAFETY: raw still belongs to the locked owner; code is a writable DWORD.
                if unsafe { GetExitCodeProcess(raw, &mut code) } == 0 {
                    return Err(last_error(&format!("Read {} exit code", self.subject)));
                }
                inner.exit_code = Some(code);
                Ok(Some(code))
            }
            WAIT_TIMEOUT => Ok(None),
            _ => Err(last_error(&format!("Wait for {}", self.subject))),
        }
    }

    /// Root plus descendants still alive in the job; zero once retired.
    pub fn active_processes(&self) -> Result<u32, String> {
        match self.inner.lock().job.as_ref() {
            Some(job) => active_processes(job),
            None => Ok(0),
        }
    }

    #[must_use]
    pub fn is_retired(&self) -> bool {
        self.inner.lock().job.is_none()
    }

    /// Kills the whole tree and waits (at most 5 seconds) for job accounting to reach zero before closing the handles.
    /// Idempotent. On failure the job handle is kept (kill-on-close still protects) and the call can be retried; never
    /// start a replacement over a tree whose retirement failed.
    pub fn terminate(&self) -> Result<(), String> {
        let mut inner = self.inner.lock();
        let Some(job) = inner.job.as_ref() else {
            return Ok(());
        };
        terminate_job(job, &self.subject, RETIRE_BOUND)?;
        if let Some(process) = inner.process.as_ref() {
            // Job accounting can reach zero a moment before the process object is signaled; the exit code is only
            // final (and the PID only dead) once it is.
            // SAFETY: the inner lock holds the process handle open during the wait and exit-code query.
            if unsafe {
                WaitForSingleObject(process.as_raw_handle(), RETIRE_BOUND.as_millis() as u32)
            } != WAIT_OBJECT_0
            {
                return Err(format!(
                    "{} process did not signal exit after its job was emptied",
                    self.subject
                ));
            }
            let mut code = 0u32;
            // SAFETY: process is still owned under the inner lock; code is a writable DWORD.
            if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } != 0 {
                inner.exit_code = Some(code);
            }
        }
        inner.process = None;
        inner.job = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_round_trip_through_msvcrt_quoting() {
        let cases: [(&str, &str); 6] = [
            ("plain", "plain"),
            ("two words", "\"two words\""),
            ("", "\"\""),
            (r#"say "hi""#, r#""say \"hi\"""#),
            (r"C:\dir with space\", r#""C:\dir with space\\""#),
            (r"back\slash", r"back\slash"),
        ];
        for (input, expected) in cases {
            let mut out = Vec::new();
            append_argument(&mut out, OsStr::new(input)).unwrap();
            assert_eq!(String::from_utf16(&out).unwrap(), expected);
        }
    }

    #[test]
    fn environment_overrides_are_case_insensitive_and_ordered() {
        let block = environment_block(
            &[(OsString::from("omp_contained_probe"), OsString::from("1"))],
            &[OsString::from("COMSPEC")],
        )
        .unwrap();
        let text = String::from_utf16_lossy(&block);
        let entries: Vec<&str> = text.split('\0').filter(|e| !e.is_empty()).collect();
        assert!(entries.contains(&"omp_contained_probe=1"));
        assert!(!entries
            .iter()
            .any(|e| e.to_uppercase().starts_with("COMSPEC=")));
        let name = |entry: &str| -> Vec<u16> {
            let split = entry[1..].find('=').map_or(entry.len(), |i| i + 1);
            entry[..split].to_uppercase().encode_utf16().collect()
        };
        assert!(entries
            .windows(2)
            .all(|pair| name(pair[0]) <= name(pair[1])));
        assert!(environment_block(&[(OsString::from("A=B"), OsString::from("x"))], &[]).is_err());
    }

    fn shell_spec(subject: &str, script: &str, capture_limit: usize) -> ContainedSpec {
        ContainedSpec {
            subject: subject.into(),
            program: PathBuf::from(std::env::var_os("COMSPEC").expect("COMSPEC")),
            args: ["/s", "/c", script].map(OsString::from).to_vec(),
            cwd: None,
            env_set: vec![(OsString::from("OMP_CONTAINED_PROBE"), OsString::from("1"))],
            env_remove: vec![],
            stdout: StdioMode::Capture,
            stderr: StdioMode::Null,
            capture_limit,
        }
    }

    #[test]
    fn contained_child_is_retired_with_its_descendants() {
        // The shell waits on a grandchild that would outlive a plain root kill; the job must take both.
        let child = ContainedChild::spawn(shell_spec(
            "retirement probe",
            "ping -n 60 127.0.0.1 >nul",
            1024,
        ))
        .unwrap();
        assert_eq!(
            process_creation_time(child.pid()),
            Some(child.creation_time_100ns())
        );
        assert_eq!(child.try_wait().unwrap(), None);
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.active_processes().unwrap() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            child.active_processes().unwrap() >= 2,
            "the shell must have started its grandchild"
        );
        child.terminate().unwrap();
        assert!(child.is_retired());
        assert_eq!(child.active_processes().unwrap(), 0);
        assert!(child.try_wait().unwrap().is_some());
        assert_eq!(process_creation_time(child.pid()), None);
        child.terminate().unwrap();
    }

    #[test]
    fn captured_output_is_bounded_and_matchable() {
        let child = ContainedChild::spawn(shell_spec(
            "capture probe",
            "echo first & echo DevTools listening on marker",
            16,
        ))
        .unwrap();
        let found = child.stdout().wait_match(Duration::from_secs(10), |text| {
            text.contains("marker").then(|| text.len())
        });
        assert!(found.is_some_and(|len| len <= 16));
        child.terminate().unwrap();
    }
}
