//! One disposable container per step. Cleanup uses a fresh cancellation token: cancelling the
//! analysis must not cancel the stop/remove operations that make the next admission safe.
use super::runtime::*;
use super::types::*;
use std::path::PathBuf;
use std::time::Instant;

pub struct JobStep {
    pub spec: ContainerSpec,
    pub copy_in: Vec<(String, PathBuf)>,
    pub copy_out: Vec<(String, PathBuf)>,
    pub timeout_secs: u64,
}

pub struct JobResult {
    pub exit_code: i64,
    pub log_tail: String,
    pub timed_out: bool,
    pub cancelled: bool,
}

struct Cleanup<'a> {
    engine: &'a dyn Engine,
    id: String,
    ctl: Ctl,
    finished: bool,
}

impl Cleanup<'_> {
    fn finish(&mut self) -> Result<()> {
        self.finished = true;
        let mut errors = Vec::new();
        match self.engine.inspect(&self.id, &self.ctl) {
            Ok(Some(info)) if !info.running => {}
            Ok(None) => return Ok(()),
            _ => {
                if let Err(e) = self.engine.stop(&self.id, 30, &self.ctl) { errors.push(e); }
            }
        }
        // Even a failed stop must attempt force-remove, but never remove volumes/networks.
        match self.engine.remove(&self.id, true, &self.ctl) {
            Ok(()) => {
                // Force removal is itself a successful stop; verify no remaining container.
                match self.engine.inspect(&self.id, &self.ctl) {
                    Ok(None) => return Ok(()),
                    Ok(Some(_)) => errors.push("Disposable container still exists after removal.".into()),
                    Err(e) => errors.push(format!("Cannot verify disposable container removal: {e}")),
                }
            }
            Err(e) => errors.push(e),
        }
        Err(format!("Analysis container {} needs reconciliation: {}", self.id, errors.join("; ")))
    }
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // Also cover an unwind in on_container; ownership was recorded before source transfer.
            let _ = self.finish();
        }
    }
}

fn append_log(text: &mut String, message: &str) {
    if !text.is_empty() { text.push('\n'); }
    text.push_str(message);
    if text.len() > 1024 * 1024 {
        let mut start = text.len() - 1024 * 1024;
        while !text.is_char_boundary(start) { start += 1; }
        text.drain(..start);
    }
}

fn redact_step(text: &mut String, step: &JobStep) {
    for (_, value) in &step.spec.secret_env {
        if !value.is_empty() { *text = text.replace(value, "[REDACTED]"); }
    }
}

pub fn run_job(engine: &dyn Engine, step: &JobStep, on_container: &dyn Fn(&str), ctl: &Ctl) -> Result<JobResult> {
    if ctl.cancelled() { return Err("Code quality scan cancelled before container creation.".into()); }
    let id = engine.create(&step.spec, ctl)?;
    let cleanup_ctl = Ctl::new(ctl.log.clone());
    let mut cleanup = Cleanup { engine, id, ctl: cleanup_ctl.clone(), finished: false };
    on_container(&cleanup.id);
    let result = (|| -> Result<JobResult> {
        for (destination, archive) in &step.copy_in {
            if ctl.cancelled() { return Err("Source transfer cancelled.".into()); }
            engine.copy_in(&cleanup.id, destination, archive, ctl)?;
        }
        if ctl.cancelled() { return Err("Scan cancelled before container start.".into()); }
        engine.start(&cleanup.id, ctl)?;
        let started = Instant::now();
        let mut log_tail = String::new();
        let mut timed_out = false;
        let mut cancelled = false;
        let exit_code = match engine.wait(&cleanup.id, step.timeout_secs, ctl) {
            Ok(code) => code,
            Err(error) => {
                cancelled = ctl.cancelled();
                let lower = error.to_lowercase();
                timed_out = !cancelled && (started.elapsed().as_secs() >= step.timeout_secs
                    || lower.contains("timed out") || lower.contains("timeout"));
                if !cancelled && !timed_out { return Err(error); }
                // Wait's cancellation intentionally stops nothing; this must finish before reports.
                engine.stop(&cleanup.id, 30, &cleanup_ctl)?;
                append_log(&mut log_tail, &error);
                engine.inspect(&cleanup.id, &cleanup_ctl)?.and_then(|info| info.exit_code).unwrap_or(-1)
            }
        };
        cancelled |= ctl.cancelled();
        if !cancelled {
            for (source, archive) in &step.copy_out {
                if ctl.cancelled() { cancelled = true; break; }
                if let Err(error) = engine.copy_out(&cleanup.id, source, archive, ctl) {
                    if ctl.cancelled() { cancelled = true; break; }
                    if exit_code == 0 && !timed_out { return Err(error); }
                    append_log(&mut log_tail, &format!("Report unavailable after unsuccessful analysis: {error}"));
                }
            }
        }
        // Logs are diagnostic only, not a substitute for a machine report.
        match engine.logs(&cleanup.id, 1000, &cleanup_ctl) {
            Ok(text) => append_log(&mut log_tail, &text),
            Err(error) => append_log(&mut log_tail, &format!("Container diagnostics unavailable: {error}")),
        }
        redact_step(&mut log_tail, step);
        Ok(JobResult { exit_code, log_tail, timed_out, cancelled })
    })();
    let result = match result {
        Err(mut error) if ctl.cancelled() => {
            redact_step(&mut error, step);
            Ok(JobResult { exit_code: -1, log_tail: error, timed_out: false, cancelled: true })
        }
        other => other,
    };
    let cleaned = cleanup.finish();
    match (result, cleaned) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(mut error), Ok(())) => { redact_step(&mut error, step); Err(error) }
        (result, Err(mut error)) => {
            if let Err(cause) = result { error = format!("{cause}; {error}"); }
            redact_step(&mut error, step);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use parking_lot::Mutex;
    use std::sync::atomic::Ordering;
    use std::path::Path;

    struct Fake {
        runtime: QualityRuntime,
        calls: Arc<Mutex<Vec<String>>>,
        fail_at: String,
        present: Mutex<bool>,
        running: Mutex<bool>,
    }
    impl Fake {
        fn new(fail_at: &str) -> Self { Self { runtime: QualityRuntime::DockerDesktop,
            calls: Arc::new(Mutex::new(Vec::new())), fail_at: fail_at.into(),
            present: Mutex::new(false), running: Mutex::new(false) } }
        fn call(&self, name: &str) -> Result<()> {
            self.calls.lock().push(name.into());
            if self.fail_at == name { Err(format!("fixture {name} failed")) } else { Ok(()) }
        }
    }
    impl Engine for Fake {
        fn runtime(&self) -> &QualityRuntime { &self.runtime }
        fn probe(&self, _: &Ctl) -> Result<EngineProbe> { Ok(EngineProbe::default()) }
        fn ensure_started(&self, _: &Ctl) -> Result<()> { Ok(()) }
        fn image_exists(&self, _: &str, _: &Ctl) -> Result<bool> { Ok(true) }
        fn pull(&self, _: &str, _: &Ctl) -> Result<()> { Ok(()) }
        fn build_image(&self, _: &str, _: &Path, _: &Ctl) -> Result<()> { Ok(()) }
        fn network_create(&self, _: &str, _: &[(String, String)], _: &Ctl) -> Result<()> { Ok(()) }
        fn volume_create(&self, _: &str, _: &[(String, String)], _: &Ctl) -> Result<()> { panic!("jobs never create persistent volumes") }
        fn create(&self, _: &ContainerSpec, _: &Ctl) -> Result<String> {
            self.call("create")?; *self.present.lock() = true; Ok("exact-id".into())
        }
        fn inspect(&self, id: &str, _: &Ctl) -> Result<Option<ContainerInfo>> {
            assert_eq!(id, "exact-id"); self.call("inspect")?;
            Ok(if *self.present.lock() { Some(ContainerInfo { id: id.into(),
                running: *self.running.lock(), exit_code: Some(0), ..ContainerInfo::default() }) } else { None })
        }
        fn list_owned(&self, _: &str, _: &Ctl) -> Result<Vec<ContainerInfo>> { panic!("job cleanup never enumerates other resources") }
        fn start(&self, _: &str, _: &Ctl) -> Result<()> { self.call("start")?; *self.running.lock() = true; Ok(()) }
        fn stop(&self, _: &str, _: u32, ctl: &Ctl) -> Result<()> {
            assert!(!ctl.cancelled()); self.call("stop")?; *self.running.lock() = false; Ok(())
        }
        fn wait(&self, _: &str, _: u64, ctl: &Ctl) -> Result<i64> {
            self.call("wait")?;
            if self.fail_at == "cancel" { ctl.cancel.store(true, Ordering::Relaxed); return Err("cancelled".into()); }
            if self.fail_at == "timeout" { return Err("Container wait timed out.".into()); }
            *self.running.lock() = false;
            Ok(if self.fail_at == "nonzero" { 2 } else { 0 })
        }
        fn copy_in(&self, _: &str, _: &str, _: &Path, _: &Ctl) -> Result<()> { self.call("copy_in") }
        fn copy_out(&self, _: &str, _: &str, _: &Path, _: &Ctl) -> Result<()> { self.call("copy_out") }
        fn remove(&self, _: &str, force: bool, ctl: &Ctl) -> Result<()> {
            assert!(force); assert!(!ctl.cancelled()); self.call("remove")?; *self.present.lock() = false; Ok(())
        }
        fn exec(&self, _: &str, _: &[String], _: &[(String, String)], _: u64, _: &Ctl) -> Result<ExecOutput> { Ok(ExecOutput::default()) }
        fn logs(&self, _: &str, _: u32, _: &Ctl) -> Result<String> { self.call("logs")?; Ok("credential-value".into()) }
    }
    fn step() -> JobStep { JobStep { spec: ContainerSpec { secret_env: vec![("TOKEN".into(), "credential-value".into())], ..ContainerSpec::default() },
        copy_in: vec![("/workspace".into(), "source.tar".into())], copy_out: vec![("/out".into(), "report.tar".into())], timeout_secs: 10 } }
    #[test]
    fn stopped_copy_start_wait_reports_then_remove() {
        let fake = Fake::new("");
        let calls = fake.calls.clone();
        let result = run_job(&fake, &step(), &|_| calls.lock().push("record".into()), &Ctl::silent()).unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(!result.log_tail.contains("credential-value"));
        assert_eq!(*fake.calls.lock(), vec!["create", "record", "copy_in", "start", "wait", "copy_out", "logs", "inspect", "remove", "inspect"]);
    }
    #[test]
    fn every_post_create_failure_always_removes_exact_container() {
        for failure in ["copy_in", "start", "wait", "copy_out", "logs", "timeout", "cancel", "nonzero"] {
            let fake = Fake::new(failure);
            let result = run_job(&fake, &step(), &|_| {}, &Ctl::silent());
            assert!(!*fake.present.lock(), "{failure}");
            let calls = fake.calls.lock();
            assert!(calls.contains(&"remove".into()), "{failure}");
            if failure == "cancel" {
                let result = result.unwrap(); assert!(result.cancelled);
                assert!(!calls.contains(&"copy_out".into()));
            } else if failure == "timeout" {
                assert!(result.unwrap().timed_out);
                assert!(calls.iter().position(|s| s == "stop").unwrap() < calls.iter().position(|s| s == "copy_out").unwrap());
            } else if failure == "nonzero" { assert_eq!(result.unwrap().exit_code, 2); }
        }
    }
    #[test]
    fn remove_attempted_even_when_stop_fails() {
        let fake = Fake::new("stop");
        *fake.present.lock() = true;
        *fake.running.lock() = true;
        let mut cleanup = Cleanup { engine: &fake, id: "exact-id".into(), ctl: Ctl::silent(), finished: false };
        cleanup.finish().unwrap();
        assert!(fake.calls.lock().contains(&"remove".into()));
    }
    #[test]
    fn callback_unwind_also_cleans_up() {
        let fake = Fake::new("");
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run_job(&fake, &step(), &|_| panic!("fixture record failure"), &Ctl::silent());
        }));
        assert!(!*fake.present.lock());
    }
}
