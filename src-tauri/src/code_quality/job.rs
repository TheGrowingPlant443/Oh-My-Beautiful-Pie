//! One disposable container per step. Cleanup uses a fresh cancellation token: cancelling the
//! analysis must not cancel the stop/remove operations that make the next admission safe.
use super::runtime::{ContainerSpec, Engine, RunControl};
use super::types::Result;
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

struct Cleanup<'engine> {
    engine: &'engine dyn Engine,
    id: String,
    control: RunControl,
    finished: bool,
}

impl Cleanup<'_> {
    fn finish(&mut self) -> Result<()> {
        self.finished = true;
        let mut errors = Vec::new();
        match self.engine.inspect(&self.id, &self.control) {
            Ok(Some(info)) if !info.running => {}
            Ok(None) => return Ok(()),
            _ => {
                if let Err(e) = self.engine.stop(&self.id, 30, &self.control) {
                    errors.push(e);
                }
            }
        }
        // Even a failed stop must attempt force-remove, but never remove volumes/networks.
        match self
            .engine
            .remove(&self.id, /* force= */ true, &self.control)
        {
            Ok(()) => {
                // Force removal is itself a successful stop; verify no remaining container.
                match self.engine.inspect(&self.id, &self.control) {
                    Ok(None) => return Ok(()),
                    Ok(Some(_)) => {
                        errors.push("Disposable container still exists after removal.".into())
                    }
                    Err(e) => {
                        errors.push(format!("Cannot verify disposable container removal: {e}"))
                    }
                }
            }
            Err(e) => errors.push(e),
        }
        Err(format!(
            "Analysis container {} needs reconciliation: {}",
            self.id,
            errors.join("; ")
        ))
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
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(message);
    if text.len() > 1024 * 1024 {
        let mut start = text.len() - 1024 * 1024;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text.drain(..start);
    }
}

fn redact_step(text: &mut String, step: &JobStep) {
    for (_, value) in &step.spec.secret_env {
        if !value.is_empty() {
            *text = text.replace(value, "[REDACTED]");
        }
    }
}

/// Waits for the container; a cancelled or timed-out wait is stopped and inspected, anything else is an error.
/// Returns `(exit_code, timed_out, cancelled)`.
fn wait_exit(
    engine: &dyn Engine,
    step: &JobStep,
    id: &str,
    log_tail: &mut String,
    control: &RunControl,
    cleanup_control: &RunControl,
) -> Result<(i64, bool, bool)> {
    let started = Instant::now();
    match engine.wait(id, step.timeout_secs, control) {
        Ok(code) => Ok((code, false, false)),
        Err(error) => {
            let cancelled = control.cancelled();
            let lower = error.to_lowercase();
            let timed_out = !cancelled
                && (started.elapsed().as_secs() >= step.timeout_secs
                    || lower.contains("timed out")
                    || lower.contains("timeout"));
            if !cancelled && !timed_out {
                return Err(error);
            }
            // Wait's cancellation intentionally stops nothing; this must finish before reports.
            engine.stop(id, 30, cleanup_control)?;
            append_log(log_tail, &error);
            let code = engine
                .inspect(id, cleanup_control)?
                .and_then(|info| info.exit_code)
                .unwrap_or(-1);
            Ok((code, timed_out, cancelled))
        }
    }
}

/// Copies every report out; returns whether a cancellation cut it short.
fn copy_out_all(
    engine: &dyn Engine,
    step: &JobStep,
    id: &str,
    exit_code: i64,
    timed_out: bool,
    log_tail: &mut String,
    control: &RunControl,
) -> Result<bool> {
    for (source, archive) in &step.copy_out {
        if control.cancelled() {
            return Ok(true);
        }
        if let Err(error) = engine.copy_out(id, source, archive, control) {
            if control.cancelled() {
                return Ok(true);
            }
            if exit_code == 0 && !timed_out {
                return Err(error);
            }
            append_log(
                log_tail,
                &format!("Report unavailable after unsuccessful analysis: {error}"),
            );
        }
    }
    Ok(false)
}

fn run_steps(
    engine: &dyn Engine,
    step: &JobStep,
    id: &str,
    control: &RunControl,
    cleanup_control: &RunControl,
) -> Result<JobResult> {
    for (destination, archive) in &step.copy_in {
        if control.cancelled() {
            return Err("Source transfer cancelled.".into());
        }
        engine.copy_in(id, destination, archive, control)?;
    }
    if control.cancelled() {
        return Err("Scan cancelled before container start.".into());
    }
    engine.start(id, control)?;
    let mut log_tail = String::new();
    let (exit_code, timed_out, mut cancelled) =
        wait_exit(engine, step, id, &mut log_tail, control, cleanup_control)?;
    cancelled |= control.cancelled();
    if !cancelled {
        cancelled = copy_out_all(
            engine,
            step,
            id,
            exit_code,
            timed_out,
            &mut log_tail,
            control,
        )?;
    }
    // Logs are diagnostic only, not a substitute for a machine report.
    match engine.logs(id, 1000, cleanup_control) {
        Ok(text) => append_log(&mut log_tail, &text),
        Err(error) => append_log(
            &mut log_tail,
            &format!("Container diagnostics unavailable: {error}"),
        ),
    }
    redact_step(&mut log_tail, step);
    Ok(JobResult {
        exit_code,
        log_tail,
        timed_out,
        cancelled,
    })
}

pub fn run_job(
    engine: &dyn Engine,
    step: &JobStep,
    on_container: &dyn Fn(&str),
    control: &RunControl,
) -> Result<JobResult> {
    if control.cancelled() {
        return Err("Code quality scan cancelled before container creation.".into());
    }
    let id = engine.create(&step.spec, control)?;
    let cleanup_control = RunControl::new(control.log.clone());
    let mut cleanup = Cleanup {
        engine,
        id,
        control: cleanup_control.clone(),
        finished: false,
    };
    on_container(&cleanup.id);
    let result = run_steps(engine, step, &cleanup.id, control, &cleanup_control);
    let result = match result {
        Err(mut error) if control.cancelled() => {
            redact_step(&mut error, step);
            Ok(JobResult {
                exit_code: -1,
                log_tail: error,
                timed_out: false,
                cancelled: true,
            })
        }
        other => other,
    };
    let cleaned = cleanup.finish();
    match (result, cleaned) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(mut error), Ok(())) => {
            redact_step(&mut error, step);
            Err(error)
        }
        (result, Err(mut error)) => {
            if let Err(cause) = result {
                error = format!("{cause}; {error}");
            }
            redact_step(&mut error, step);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::runtime::{ContainerInfo, EngineProbe, ExecOutput};
    use super::super::types::QualityRuntime;
    use super::*;
    use parking_lot::Mutex;
    use std::path::Path;
    use std::sync::atomic::Ordering;

    struct Fake {
        runtime: QualityRuntime,
        calls: Mutex<Vec<String>>,
        fail_at: String,
        present: Mutex<bool>,
        running: Mutex<bool>,
    }
    impl Fake {
        fn new(fail_at: &str) -> Self {
            Self {
                runtime: QualityRuntime::DockerDesktop,
                calls: Mutex::new(Vec::new()),
                fail_at: fail_at.into(),
                present: Mutex::new(false),
                running: Mutex::new(false),
            }
        }
        fn call(&self, name: &str) -> Result<()> {
            self.calls.lock().push(name.into());
            if self.fail_at == name {
                Err(format!("fixture {name} failed"))
            } else {
                Ok(())
            }
        }
    }
    impl Engine for Fake {
        fn runtime(&self) -> &QualityRuntime {
            &self.runtime
        }
        fn probe(&self, _: &RunControl) -> Result<EngineProbe> {
            Ok(EngineProbe::default())
        }
        fn ensure_started(&self, _: &RunControl) -> Result<()> {
            Ok(())
        }
        fn image_exists(&self, _: &str, _: &RunControl) -> Result<bool> {
            Ok(true)
        }
        fn pull(&self, _: &str, _: &RunControl) -> Result<()> {
            Ok(())
        }
        fn build_image(&self, _: &str, _: &Path, _: &RunControl) -> Result<()> {
            Ok(())
        }
        fn network_create(&self, _: &str, _: &[(String, String)], _: &RunControl) -> Result<()> {
            Ok(())
        }
        fn volume_create(&self, _: &str, _: &[(String, String)], _: &RunControl) -> Result<()> {
            panic!("jobs never create persistent volumes")
        }
        fn create(&self, _: &ContainerSpec, _: &RunControl) -> Result<String> {
            self.call("create")?;
            *self.present.lock() = true;
            Ok("exact-id".into())
        }
        fn inspect(&self, id: &str, _: &RunControl) -> Result<Option<ContainerInfo>> {
            assert_eq!(id, "exact-id");
            self.call("inspect")?;
            Ok(if *self.present.lock() {
                Some(ContainerInfo {
                    id: id.into(),
                    running: *self.running.lock(),
                    exit_code: Some(0),
                    ..ContainerInfo::default()
                })
            } else {
                None
            })
        }
        fn list_owned(&self, _: &str, _: &RunControl) -> Result<Vec<ContainerInfo>> {
            panic!("job cleanup never enumerates other resources")
        }
        fn start(&self, _: &str, _: &RunControl) -> Result<()> {
            self.call("start")?;
            *self.running.lock() = true;
            Ok(())
        }
        fn stop(&self, _: &str, _: u32, control: &RunControl) -> Result<()> {
            assert!(!control.cancelled());
            self.call("stop")?;
            *self.running.lock() = false;
            Ok(())
        }
        fn wait(&self, _: &str, _: u64, control: &RunControl) -> Result<i64> {
            self.call("wait")?;
            if self.fail_at == "cancel" {
                control.cancel.store(true, Ordering::Relaxed);
                return Err("cancelled".into());
            }
            if self.fail_at == "timeout" {
                return Err("Container wait timed out.".into());
            }
            *self.running.lock() = false;
            Ok(if self.fail_at == "nonzero" { 2 } else { 0 })
        }
        fn copy_in(&self, _: &str, _: &str, _: &Path, _: &RunControl) -> Result<()> {
            self.call("copy_in")
        }
        fn copy_out(&self, _: &str, _: &str, _: &Path, _: &RunControl) -> Result<()> {
            self.call("copy_out")
        }
        fn remove(&self, _: &str, force: bool, control: &RunControl) -> Result<()> {
            assert!(force);
            assert!(!control.cancelled());
            self.call("remove")?;
            *self.present.lock() = false;
            Ok(())
        }
        fn exec(
            &self,
            _: &str,
            _: &[String],
            _: &[(String, String)],
            _: u64,
            _: &RunControl,
        ) -> Result<ExecOutput> {
            Ok(ExecOutput::default())
        }
        fn logs(&self, _: &str, _: u32, _: &RunControl) -> Result<String> {
            self.call("logs")?;
            Ok("credential-value".into())
        }
    }
    fn step() -> JobStep {
        JobStep {
            spec: ContainerSpec {
                secret_env: vec![("TOKEN".into(), "credential-value".into())],
                ..ContainerSpec::default()
            },
            copy_in: vec![("/workspace".into(), "source.tar".into())],
            copy_out: vec![("/out".into(), "report.tar".into())],
            timeout_secs: 10,
        }
    }
    #[test]
    fn stopped_copy_start_wait_reports_then_remove() {
        let fake = Fake::new("");
        let calls = &fake.calls;
        let result = run_job(
            &fake,
            &step(),
            &|_| calls.lock().push("record".into()),
            &RunControl::silent(),
        )
        .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(!result.log_tail.contains("credential-value"));
        assert_eq!(
            *fake.calls.lock(),
            vec![
                "create", "record", "copy_in", "start", "wait", "copy_out", "logs", "inspect",
                "remove", "inspect"
            ]
        );
    }
    #[test]
    fn every_post_create_failure_always_removes_exact_container() {
        for failure in [
            "copy_in", "start", "wait", "copy_out", "logs", "timeout", "cancel", "nonzero",
        ] {
            let fake = Fake::new(failure);
            let result = run_job(&fake, &step(), &|_| {}, &RunControl::silent());
            assert!(!*fake.present.lock(), "{failure}");
            let calls = fake.calls.lock();
            assert!(calls.contains(&"remove".into()), "{failure}");
            if failure == "cancel" {
                let result = result.unwrap();
                assert!(result.cancelled);
                assert!(!calls.contains(&"copy_out".into()));
            } else if failure == "timeout" {
                assert!(result.unwrap().timed_out);
                assert!(
                    calls.iter().position(|s| s == "stop").unwrap()
                        < calls.iter().position(|s| s == "copy_out").unwrap()
                );
            } else if failure == "nonzero" {
                assert_eq!(result.unwrap().exit_code, 2);
            }
        }
    }
    #[test]
    fn remove_attempted_even_when_stop_fails() {
        let fake = Fake::new("stop");
        *fake.present.lock() = true;
        *fake.running.lock() = true;
        let mut cleanup = Cleanup {
            engine: &fake,
            id: "exact-id".into(),
            control: RunControl::silent(),
            finished: false,
        };
        cleanup.finish().unwrap();
        assert!(fake.calls.lock().contains(&"remove".into()));
    }
    #[test]
    fn callback_unwind_also_cleans_up() {
        let fake = Fake::new("");
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run_job(
                &fake,
                &step(),
                &|_| panic!("fixture record failure"),
                &RunControl::silent(),
            );
        }));
        assert!(!*fake.present.lock());
    }
}
