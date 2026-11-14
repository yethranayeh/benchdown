//! `benchdown session`: prepare the Mac, run jobs, and restore every changed setting.
//!
//! The session reads each setting's prior state, changes only what was asked for,
//! and restores it on every exit path. A small root guard applies the changes over
//! a pipe and restores by itself if the session disappears. A restore state file
//! covers a crash or power cut that takes the guard down too.

#[cfg(test)]
pub mod fakes;
pub mod format;
pub mod guard;
pub mod jobs;
pub mod restore;
pub mod sampler;
pub mod settings;
pub mod state;
pub mod time;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::cli::SessionRunPlan;
use crate::clock::Clock;
use crate::runner::{CaptureOptions, Captured, Job, JobOptions, Runner, System};
use crate::session::guard::{Guard, GuardPaths};
use crate::session::jobs::{JobSpec, job_argv};
use crate::session::restore::{RestoreOptions, RestoreResult};
use crate::session::sampler::{
    ActiveSampler, SAMPLES_FILE_NAME, SESSION_SAMPLES_ENV, SamplerOptions, start_sampler,
};
use crate::session::settings::{PlannedChange, SettingFlags, SettingPlan, plan_setting_changes};
use crate::session::state::{
    RestoreEntry, RestoreState, remove as remove_state, write as write_state,
};

pub use crate::session::sampler::DEFAULT_SAMPLE_SECONDS;

/// What a person runs to undo a session that died before it could restore.
pub const RESTORE_COMMAND: &str = "benchdown session restore";

/// The default root under the working directory.
pub const DEFAULT_OUT_DIR: &str = "benchdown-sessions";

pub const SESSION_LOG: &str = "session.log";
pub const STATUS_FILE: &str = "status.txt";
pub const GUARD_LOG: &str = "guard.log";
pub const GUARD_SCRIPT: &str = "guard.sh";

/// The sudo ticket is refreshed at this interval while the session runs.
pub const DEFAULT_SUDO_REFRESH_MS: u64 = 60_000;

const MILLISECONDS_PER_MINUTE: f64 = 60_000.0;

/// A signal the session stopped on, with the exit code a shell reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSignal {
    Interrupt,
    Terminate,
    Hangup,
}

impl SessionSignal {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Interrupt => 130,
            Self::Terminate => 143,
            Self::Hangup => 129,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
            Self::Hangup => "SIGHUP",
        }
    }

    fn from_exit_code(code: u8) -> Option<Self> {
        match code {
            130 => Some(Self::Interrupt),
            143 => Some(Self::Terminate),
            129 => Some(Self::Hangup),
            _ => None,
        }
    }
}

/// One signal callback.
pub type SignalHandler = Box<dyn Fn(SessionSignal) + Send + Sync>;

/// Where the session hears about a terminal hangup or a stop.
pub trait SignalSource: Send + Sync {
    fn on(&self, handler: SignalHandler);
}

/// The real signals, watched on a background thread.
pub struct SystemSignals {
    handlers: Arc<Mutex<Vec<SignalHandler>>>,
}

impl SystemSignals {
    pub fn new() -> Result<Self, String> {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

        let handlers: Arc<Mutex<Vec<SignalHandler>>> = Arc::new(Mutex::new(Vec::new()));
        let registered = Arc::clone(&handlers);
        let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP])
            .map_err(|error| format!("could not watch for signals: {error}"))?;
        std::thread::spawn(move || {
            for signal in signals.forever() {
                let Some(mapped) = (match signal {
                    SIGINT => Some(SessionSignal::Interrupt),
                    SIGTERM => Some(SessionSignal::Terminate),
                    SIGHUP => Some(SessionSignal::Hangup),
                    _ => None,
                }) else {
                    continue;
                };
                if let Ok(handlers) = registered.lock() {
                    for handler in handlers.iter() {
                        handler(mapped);
                    }
                }
            }
        });
        Ok(Self { handlers })
    }
}

impl SignalSource for SystemSignals {
    fn on(&self, handler: SignalHandler) {
        if let Ok(mut handlers) = self.handlers.lock() {
            handlers.push(handler);
        }
    }
}

/// The timestamped session log, written to the terminal and to `session.log`.
#[derive(Clone)]
struct Log {
    write: Arc<dyn Fn(&str) + Send + Sync>,
    path: Option<PathBuf>,
    clock: Arc<dyn Clock>,
    failed: Arc<Mutex<bool>>,
}

impl Log {
    fn new(
        write: Arc<dyn Fn(&str) + Send + Sync>,
        path: Option<PathBuf>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            write,
            path,
            clock,
            failed: Arc::new(Mutex::new(false)),
        }
    }

    fn line(&self, text: &str) {
        for part in text.split('\n') {
            self.write_line(part);
        }
    }

    fn write_line(&self, text: &str) {
        let rendered = format!("[{}] {text}", time::clock(self.clock.now_ms()));
        (self.write)(&format!("{rendered}\n"));
        let Some(path) = &self.path else {
            return;
        };
        let failed = self.failed.lock().map(|failed| *failed).unwrap_or(true);
        if failed {
            return;
        }
        if let Err(error) = append_line(path, &rendered) {
            if let Ok(mut failed) = self.failed.lock() {
                *failed = true;
            }
            self.write_line(&format!(
                "[log] could not write {}: {error}",
                path.display()
            ));
        }
    }
}

fn append_line(path: &Path, text: &str) -> Result<(), std::io::Error> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    writeln!(file, "{text}")
}

/// The one-line status file in the session folder and at the output root.
struct Status {
    path: PathBuf,
    out_path: PathBuf,
    session_id: String,
}

impl Status {
    fn set(&self, text: &str) {
        let line = format!("benchdown session {}: {text}\n", self.session_id);
        let _ = std::fs::write(&self.path, &line);
        let _ = std::fs::write(&self.out_path, &line);
    }
}

/// Everything one session needs. Tests inject a fake runner and signal source.
#[derive(Clone)]
pub struct SessionOptions {
    pub runner: Arc<dyn Runner>,
    pub clock: Arc<dyn Clock>,
    pub settings: SettingFlags,
    pub jobs: Vec<JobSpec>,
    pub stop_on_failure: bool,
    pub dry_run: bool,
    pub out_dir: PathBuf,
    pub cwd: PathBuf,
    pub state_path: PathBuf,
    pub session_id: String,
    pub started_at_ms: u64,
    pub sample_seconds: u64,
    pub sudo_refresh_ms: u64,
    pub notify: bool,
    pub write: Arc<dyn Fn(&str) + Send + Sync>,
    pub signals: Arc<dyn SignalSource>,
}

/// One finished job.
#[derive(Debug, Clone, PartialEq)]
pub struct JobResult {
    pub job: JobSpec,
    pub exit_code: i32,
    pub duration_ms: u64,
}

/// What one session did.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SessionOutcome {
    pub exit_code: i32,
    pub session_dir: PathBuf,
    pub log_path: PathBuf,
    pub status_path: PathBuf,
    pub jobs: Vec<JobResult>,
    pub restores: Vec<RestoreResult>,
    pub restored_all: bool,
    pub session_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Change(usize),
    Job(usize),
}

/// Runs one session from start to restore.
pub fn run_session(options: SessionOptions) -> SessionOutcome {
    let session_dir = options.out_dir.join("sessions").join(&options.session_id);
    let log_path = session_dir.join(SESSION_LOG);
    let status_path = session_dir.join(STATUS_FILE);
    let out_status_path = options.out_dir.join(STATUS_FILE);
    let guard_log_path = session_dir.join(GUARD_LOG);
    let guard_script_path = session_dir.join(GUARD_SCRIPT);
    let samples_path = session_dir.join(SAMPLES_FILE_NAME);

    let mut outcome = SessionOutcome {
        exit_code: 0,
        session_dir: session_dir.clone(),
        log_path: log_path.clone(),
        status_path: status_path.clone(),
        jobs: Vec::new(),
        restores: Vec::new(),
        restored_all: true,
        session_error: None,
    };

    if options.state_path.exists() {
        let message = "restore state already exists";
        (options.write)("A previous session did not restore its changes.\n");
        (options.write)(&format!("Run: {RESTORE_COMMAND}\n"));
        (options.write)(&format!("State file: {}\n", options.state_path.display()));
        outcome.exit_code = 1;
        outcome.restored_all = false;
        outcome.session_error = Some(message.to_owned());
        return outcome;
    }

    if !options.dry_run {
        if let Err(error) = std::fs::create_dir_all(&session_dir) {
            let message = format!("{}: {error}", session_dir.display());
            (options.write)(&format!("Session error: {message}\n"));
            outcome.exit_code = 1;
            outcome.restored_all = false;
            outcome.session_error = Some(message);
            return outcome;
        }
        let _ = std::fs::write(&log_path, "");
        let _ = std::fs::write(&guard_log_path, "");
    }

    let log = Log::new(
        Arc::clone(&options.write),
        (!options.dry_run).then(|| log_path.clone()),
        Arc::clone(&options.clock),
    );
    let status = (!options.dry_run).then(|| Status {
        path: status_path.clone(),
        out_path: out_status_path.clone(),
        session_id: options.session_id.clone(),
    });
    log.line(&format!(
        "Session {} started {}",
        options.session_id,
        time::date_time(options.started_at_ms)
    ));
    log.line(&format!("Working directory: {}", options.cwd.display()));

    let interrupted = Arc::new(AtomicU8::new(0));
    let current_job: Arc<Mutex<Option<Arc<dyn Job>>>> = Arc::new(Mutex::new(None));
    {
        let interrupted = Arc::clone(&interrupted);
        let current = Arc::clone(&current_job);
        options.signals.on(Box::new(move |signal| {
            if interrupted.swap(signal.exit_code() as u8, Ordering::SeqCst) != 0 {
                return;
            }
            if let Ok(slot) = current.lock() {
                if let Some(job) = slot.as_ref() {
                    job.stop();
                }
            }
        }));
    }

    let mut job_results: Vec<JobResult> = Vec::new();
    let mut restore_results: Vec<RestoreResult> = Vec::new();
    let mut planned_changes: Vec<PlannedChange> = Vec::new();
    let mut state_written = false;
    let mut guard: Option<Guard> = None;
    let mut guard_usable = false;
    let mut keepalive: Option<SudoKeepalive> = None;
    let mut sampler: Option<Box<dyn ActiveSampler>> = None;
    let mut caffeinate: Option<u32> = None;
    let mut change_failures = 0usize;
    let mut session_error: Option<String> = None;

    let body: Result<(), String> = (|| {
        if options.dry_run {
            let plan = plan_setting_changes(options.runner.as_ref(), options.settings, false);
            planned_changes = plan.changes.clone();
            let steps = build_steps(&planned_changes, &options.jobs);
            print_plan(
                &log,
                &options,
                &steps,
                &plan,
                &planned_changes,
                &log_path,
                &status_path,
                &guard_script_path,
                &guard_log_path,
                &samples_path,
            );
            log.line("Dry run complete. Nothing was changed and no password was requested.");
            return Ok(());
        }

        log.line("Asking for your password once with sudo -v");
        let sudo = ["sudo", "-v"].map(str::to_owned);
        let sudo_code = options.runner.interactive(&sudo)?;
        if sudo_code != 0 {
            return Err(format!("sudo was not granted (exit {sudo_code})"));
        }

        let own_pid = std::process::id().to_string();
        let caffeinate_argv = ["caffeinate", "-dimsu", "-w", &own_pid]
            .map(str::to_owned)
            .to_vec();
        match options.runner.spawn(&caffeinate_argv) {
            Ok(spawned) => {
                caffeinate = Some(spawned.pid);
                log.line(&format!(
                    "Keeping the Mac awake with caffeinate -dimsu -w {own_pid}"
                ));
            }
            Err(error) => log.line(&format!("Could not start caffeinate: {error}")),
        }
        keepalive = Some(start_sudo_keepalive(
            Arc::clone(&options.runner),
            Duration::from_millis(options.sudo_refresh_ms.max(1)),
            log.clone(),
        ));

        let sampler_log = log.clone();
        sampler = Some(start_sampler(SamplerOptions {
            runner: Arc::clone(&options.runner),
            clock: Arc::clone(&options.clock),
            path: samples_path.clone(),
            interval_ms: options.sample_seconds.saturating_mul(1_000),
            on_first_failure: Arc::new(move |message| {
                sampler_log.line(&format!(
                    "A load sample failed ({message}), sampling continues"
                ));
            }),
        })?);
        log.line(&format!(
            "Sampling load and the busiest processes every {} s",
            options.sample_seconds
        ));

        let plan = plan_setting_changes(options.runner.as_ref(), options.settings, true);
        planned_changes = plan.changes.clone();
        let steps = build_steps(&planned_changes, &options.jobs);
        print_plan(
            &log,
            &options,
            &steps,
            &plan,
            &planned_changes,
            &log_path,
            &status_path,
            &guard_script_path,
            &guard_log_path,
            &samples_path,
        );
        if let Some(status) = &status {
            status.set(&format!(
                "Step 0/{}: starting, next: {}",
                steps.len(),
                steps
                    .first()
                    .map(|step| step_label(*step, &planned_changes, &options.jobs))
                    .unwrap_or_else(|| "Restore system settings".to_owned())
            ));
        }

        if !planned_changes.is_empty() {
            guard::write_script(&guard_script_path)?;
            let paths = GuardPaths {
                guard_log: &guard_log_path,
                state: &options.state_path,
                status: &status_path,
                out_status: &out_status_path,
            };
            let started = Guard::start(
                options.runner.as_ref(),
                &guard_script_path,
                &paths,
                Some(&options.cwd),
            )?;
            log.line(&format!(
                "Started the restore guard, log: {}",
                guard_log_path.display()
            ));
            write_state(
                &options.state_path,
                &build_state(&options, &planned_changes),
            )?;
            state_written = true;
            guard_usable = true;
            guard = Some(started);
            log.line(&format!(
                "Wrote restore state to {}",
                options.state_path.display()
            ));
        }

        let total = steps.len();
        for (index, step) in steps.iter().enumerate() {
            if let Some(signal) = SessionSignal::from_exit_code(interrupted.load(Ordering::SeqCst))
            {
                log.line(&format!("Received {}, stopping", signal.label()));
                break;
            }
            let number = index + 1;
            let next_label = steps
                .get(index + 1)
                .map(|next| step_label(*next, &planned_changes, &options.jobs))
                .unwrap_or_else(|| "Restore system settings".to_owned());
            match step {
                Step::Change(change_index) => {
                    let mut change = planned_changes[*change_index].clone();
                    if let Some(status) = &status {
                        status.set(&format!(
                            "Step {number}/{total}: {}, next: {next_label}",
                            change.label
                        ));
                    }
                    log.line(&format!("[{number}/{total}] {}", change.label));
                    log.line(&format!("  Command: {}", change.apply_command));
                    if state_written {
                        write_state(
                            &options.state_path,
                            &build_state(&options, &planned_changes),
                        )
                        .map_err(|error| {
                            format!(
                                "could not update the restore state before {}: {error}",
                                change.label
                            )
                        })?;
                    }
                    let result = apply_change(
                        &options,
                        &mut guard,
                        &mut guard_usable,
                        &change,
                        &planned_changes,
                        state_written,
                        &log,
                    )?;
                    change.applied = Some(true);
                    change.applied_exit_code = Some(result.exit_code);
                    planned_changes[*change_index] = change;
                    if state_written {
                        match write_state(
                            &options.state_path,
                            &build_state(&options, &planned_changes),
                        ) {
                            Ok(()) => log.line("  Updated the restore state file"),
                            Err(error) => log.line(&format!(
                                "  Could not update the restore state file: {error}"
                            )),
                        }
                    }
                    if result.exit_code == 0 {
                        log.line(&format!("[{number}/{total}] exit 0"));
                    } else {
                        change_failures += 1;
                        log.line(&format!("[{number}/{total}] exit {}", result.exit_code));
                        if !result.stderr.trim().is_empty() {
                            log.line(result.stderr.trim());
                        }
                    }
                }
                Step::Job(job_index) => {
                    let job = &options.jobs[*job_index];
                    let result = run_job(
                        &options,
                        job,
                        number,
                        total,
                        &next_label,
                        &status,
                        &log,
                        &current_job,
                        &interrupted,
                        &samples_path,
                    );
                    let failed = result.exit_code != 0;
                    job_results.push(result);
                    if failed && options.stop_on_failure {
                        log.line(
                            "Stopping after the failed job because --stop-on-failure was given",
                        );
                        break;
                    }
                }
            }
        }
        Ok(())
    })();

    if let Err(error) = body {
        log.line(&format!("Session error: {error}"));
        session_error = Some(error);
    }

    if let Some(mut keepalive) = keepalive.take() {
        keepalive.stop();
    }
    if let Some(mut sampler) = sampler.take() {
        sampler.stop();
    }

    let mut restored_all = true;
    if !options.dry_run {
        if state_written {
            log.line(&format!(
                "Restoring {} changed system setting(s)",
                planned_changes.len()
            ));
            if let Some(status) = &status {
                status.set("Restoring system settings");
            }
            let total = planned_changes.len();
            for (index, change) in planned_changes.iter().enumerate().rev() {
                let number = total - index;
                log.line(&format!(
                    "[restore {number}/{total}] {}: {}",
                    change.label, change.undo_command
                ));
                if let Some(status) = &status {
                    status.set(&format!("Restoring {}", change.label));
                }
                let result = undo_change(
                    &options,
                    &mut guard,
                    &mut guard_usable,
                    change,
                    &planned_changes,
                    state_written,
                    &log,
                );
                let ok = result.exit_code == 0;
                if ok {
                    log.line(&format!("[restore {number}/{total}] exit 0"));
                } else {
                    log.line(&format!(
                        "[restore {number}/{total}] exit {} (failed)",
                        result.exit_code
                    ));
                    if !result.stderr.trim().is_empty() {
                        log.line(result.stderr.trim());
                    }
                }
                restore_results.push(RestoreResult {
                    entry: RestoreEntry::from(change),
                    exit_code: result.exit_code,
                    stderr: result.stderr,
                    ok,
                    error: None,
                });
            }
            if restore_results.iter().all(|result| result.ok) {
                match remove_state(&options.state_path) {
                    Ok(()) => log.line(&format!(
                        "Removed restore state {}",
                        options.state_path.display()
                    )),
                    Err(error) => {
                        restored_all = false;
                        log.line(&format!("Could not remove restore state: {error}"));
                    }
                }
            } else {
                restored_all = false;
                if state_written {
                    if let Err(error) = write_state(
                        &options.state_path,
                        &build_state(&options, &planned_changes),
                    ) {
                        log.line(&format!("Could not rewrite the restore state: {error}"));
                    }
                }
            }
        } else {
            log.line("No system settings were changed, so nothing needs restoring");
        }

        if !restored_all {
            log.line("Not every system setting was restored");
            log_manual_commands(&planned_changes, &log);
            log.line(&format!(
                "State file kept at {}",
                options.state_path.display()
            ));
        }

        if let Some(mut guard) = guard.take() {
            let closed = if guard_usable {
                guard.finish()
            } else {
                guard.close()
            };
            match closed {
                Ok(closed) if closed.exit_code == 0 => log.line("Restore guard exited"),
                Ok(closed) => log.line(&format!(
                    "Restore guard exited {}{}",
                    closed.exit_code,
                    stderr_detail(&closed.stderr)
                )),
                Err(error) => log.line(&format!("Could not close the restore guard: {error}")),
            }
        }

        if let Some(pid) = caffeinate {
            let kill = ["kill", "-TERM", &pid.to_string()].map(str::to_owned);
            if options
                .runner
                .capture(&kill, CaptureOptions::default())
                .map(|result| !result.succeeded())
                .unwrap_or(true)
            {
                log.line("Could not stop caffeinate");
            }
        }
    }

    let applied_ok = change_failures == 0;
    let jobs_ok = job_results.iter().all(|result| result.exit_code == 0);
    let mut exit_code = if session_error.is_some() || !applied_ok || !jobs_ok || !restored_all {
        1
    } else {
        0
    };
    if let Some(signal) = SessionSignal::from_exit_code(interrupted.load(Ordering::SeqCst)) {
        if !options.dry_run {
            exit_code = signal.exit_code();
        }
    }

    if !options.dry_run {
        log.line("");
        log.line("Summary");
        for result in &job_results {
            let outcome = if result.exit_code == 0 {
                "ok".to_owned()
            } else {
                format!("failed (exit {})", result.exit_code)
            };
            log.line(&format!(
                "  Job {}: {outcome}, {}",
                result.job.name,
                format::duration(result.duration_ms)
            ));
        }
        for job in options.jobs.iter().skip(job_results.len()) {
            log.line(&format!("  Job {}: did not run", job.name));
        }
        log.line(&format!(
            "  System changes: {} attempted",
            planned_changes
                .iter()
                .filter(|change| change.applied.is_some())
                .count()
        ));
        if restore_results.is_empty() {
            log.line("  Restores: none needed");
        } else {
            for result in &restore_results {
                let outcome = if result.ok {
                    "ok".to_owned()
                } else {
                    format!("failed (exit {})", result.exit_code)
                };
                log.line(&format!("  Restore {}: {outcome}", result.entry.label));
            }
        }
        log.line(&format!(
            "  Restore state file: {}",
            if !state_written {
                "not needed"
            } else if restored_all {
                "removed"
            } else {
                "kept"
            }
        ));
        log.line(&format!("  Log: {}", log_path.display()));
        log.line(&format!("  Session: {}", session_dir.display()));
        log.line(&format!(
            "  Result: {}",
            if exit_code == 0 { "success" } else { "failure" }
        ));
    }

    if !options.dry_run {
        if let Some(status) = &status {
            let restores = if restore_results.is_empty() {
                "none needed".to_owned()
            } else {
                format!(
                    "{} of {} ok",
                    restore_results.iter().filter(|result| result.ok).count(),
                    restore_results.len()
                )
            };
            status.set(&format!(
                "Finished: {}. Jobs: {}. Restores: {restores}. Log: {} Session: {}",
                if exit_code == 0 {
                    "all jobs succeeded, everything restored"
                } else {
                    "finished with failures, see the log"
                },
                summarize_jobs(&job_results),
                log_path.display(),
                session_dir.display()
            ));
        }
        if options.notify {
            send_notification(
                options.runner.as_ref(),
                &log,
                &job_results,
                options.jobs.len(),
                restored_all,
                change_failures,
                session_error.as_deref(),
                SessionSignal::from_exit_code(interrupted.load(Ordering::SeqCst)),
            );
        }
    }

    outcome.exit_code = exit_code;
    outcome.jobs = job_results;
    outcome.restores = restore_results;
    outcome.restored_all = restored_all;
    outcome.session_error = session_error;
    outcome
}

/// The session plan the command line resolved, run with the real machine.
pub fn run(plan: &SessionRunPlan, system: &System) -> i32 {
    if is_root(system.runner.as_ref()) {
        eprintln!("benchdown session refuses to run as root. Run it as your own user.");
        return 2;
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let out_dir = plan
        .out
        .clone()
        .unwrap_or_else(|| cwd.join(DEFAULT_OUT_DIR));
    let out_dir = if out_dir.is_absolute() {
        out_dir
    } else {
        cwd.join(out_dir)
    };
    let started_at_ms = system.clock.now_ms();
    let session_id = session_folder_id(&out_dir, started_at_ms);
    let signals = match SystemSignals::new() {
        Ok(signals) => Arc::new(signals),
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let write: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(|text: &str| {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
    });
    let outcome = run_session(SessionOptions {
        runner: Arc::clone(&system.runner),
        clock: Arc::clone(&system.clock),
        settings: plan.settings,
        jobs: plan.jobs.clone(),
        stop_on_failure: plan.stop_on_failure,
        dry_run: plan.dry_run,
        out_dir,
        cwd,
        state_path: state::resolve_state_path(),
        session_id,
        started_at_ms,
        sample_seconds: plan.sample_seconds,
        sudo_refresh_ms: DEFAULT_SUDO_REFRESH_MS,
        notify: true,
        write,
        signals,
    });
    if let Some(error) = &outcome.session_error {
        eprintln!("error: {error}");
    }
    outcome.exit_code
}

/// `benchdown session restore`, with the real machine.
pub fn restore(system: &System) -> i32 {
    let state_path = state::resolve_state_path();
    let write = |text: &str| {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
    };
    restore::run_restore(RestoreOptions {
        runner: system.runner.as_ref(),
        state_path: &state_path,
        write: &write,
    })
}

/// `<timestamp>` or `<timestamp>-1`, `-2` when that session folder already exists.
pub fn session_folder_id(out_dir: &Path, started_at_ms: u64) -> String {
    let base = time::session_id(started_at_ms);
    let mut candidate = base.clone();
    let mut suffix = 1;
    while out_dir.join("sessions").join(&candidate).exists() {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

fn build_steps(changes: &[PlannedChange], jobs: &[JobSpec]) -> Vec<Step> {
    (0..changes.len())
        .map(Step::Change)
        .chain((0..jobs.len()).map(Step::Job))
        .collect()
}

fn step_label(step: Step, changes: &[PlannedChange], jobs: &[JobSpec]) -> String {
    match step {
        Step::Change(index) => changes[index].label.clone(),
        Step::Job(index) => format!("Job {}", jobs[index].name),
    }
}

#[allow(clippy::too_many_arguments)]
fn print_plan(
    log: &Log,
    options: &SessionOptions,
    steps: &[Step],
    plan: &SettingPlan,
    changes: &[PlannedChange],
    log_path: &Path,
    status_path: &Path,
    guard_path: &Path,
    guard_log_path: &Path,
    samples_path: &Path,
) {
    log.line(&format!("Plan for session {}", options.session_id));
    log.line(&format!("Log: {}", log_path.display()));
    log.line(&format!("Status: {}", status_path.display()));
    log.line(&format!(
        "Restore guard: /bin/sh {} (starts when a system change is planned)",
        guard_path.display()
    ));
    log.line(&format!("Guard log: {}", guard_log_path.display()));
    log.line(&format!(
        "Samples: {} (every {} s)",
        samples_path.display(),
        options.sample_seconds
    ));
    if options.dry_run {
        log.line("Dry run: the plan below is not executed, no password is requested");
    }
    log.line("");
    log.line(&format!(
        "Plan ({} {})",
        steps.len(),
        if steps.len() == 1 { "step" } else { "steps" }
    ));
    for (index, step) in steps.iter().enumerate() {
        let number = index + 1;
        match step {
            Step::Change(change_index) => {
                let change = &changes[*change_index];
                log.line(&format!(
                    "  {number}. {}. Command: {}. Undo: {}",
                    change.label, change.apply_command, change.undo_command
                ));
            }
            Step::Job(job_index) => {
                let job = &options.jobs[*job_index];
                log.line(&format!(
                    "  {number}. Job {}: {} ({})",
                    job.name,
                    job.command,
                    estimate_text(job.estimate_minutes)
                ));
            }
        }
    }
    log.line("");
    log.line(&format!("System changes: {}", changes.len()));
    let (total_minutes, all_estimated) = estimate_jobs(&options.jobs);
    log.line(&format!(
        "Jobs: {}, {}",
        options.jobs.len(),
        total_estimate_text(total_minutes, all_estimated)
    ));
    let expected_end = expected_end(options, total_minutes, all_estimated);
    log.line(&format!(
        "Expected end: {}",
        match expected_end {
            Some(end) => time::date_time(end),
            None => "unknown, a job has no estimate".to_owned(),
        }
    ));
    for note in &plan.notes {
        log.line(&format!("Note: {note}"));
    }
    if !plan.reads.is_empty() {
        log.line("");
        log.line("Prior state reads (read-only):");
        for read in &plan.reads {
            log.line(&format!("  {read}"));
        }
    }
    log.line("");
    log.line("Preparation commands (not settings):");
    log.line("  sudo -v");
    log.line(&format!("  caffeinate -dimsu -w {}", std::process::id()));
    log.line(&format!(
        "  sudo -n -v every {} seconds",
        DEFAULT_SUDO_REFRESH_MS / 1_000
    ));
    if !changes.is_empty() {
        log.line("Manual undo commands:");
        for change in changes.iter().rev() {
            log.line(&format!("  {}", change.undo_command));
        }
    }
    log.line(&format!(
        "If this session dies without restoring, run: {RESTORE_COMMAND}"
    ));
    log.line("");
}

fn estimate_text(minutes: Option<f64>) -> String {
    match minutes {
        Some(minutes) => format!("estimate {}", format::estimate(minutes)),
        None => "no estimate".to_owned(),
    }
}

fn estimate_jobs(jobs: &[JobSpec]) -> (f64, bool) {
    let mut total = 0.0;
    let mut all = true;
    for job in jobs {
        match job.estimate_minutes {
            Some(minutes) => total += minutes,
            None => all = false,
        }
    }
    (total, all)
}

fn expected_end(options: &SessionOptions, total_minutes: f64, all_estimated: bool) -> Option<u64> {
    all_estimated
        .then(|| options.started_at_ms + (total_minutes * MILLISECONDS_PER_MINUTE).round() as u64)
}

fn total_estimate_text(total_minutes: f64, all_estimated: bool) -> String {
    if all_estimated {
        format!("total estimate {}", format::estimate(total_minutes))
    } else if total_minutes > 0.0 {
        format!(
            "total estimate at least {}",
            format::estimate(total_minutes)
        )
    } else {
        "no job has an estimate".to_owned()
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_change(
    options: &SessionOptions,
    guard: &mut Option<Guard>,
    guard_usable: &mut bool,
    change: &PlannedChange,
    changes: &[PlannedChange],
    state_written: bool,
    log: &Log,
) -> Result<Captured, String> {
    if *guard_usable {
        if let Some(running) = guard.as_mut() {
            match running.request(&format!("apply {}", change.guard)) {
                Ok(reply) => return Ok(reply),
                Err(error) => {
                    *guard_usable = false;
                    log.line(&format!(
                        "The restore guard stopped ({error}), changing settings directly with sudo"
                    ));
                    // Closing the guard makes it restore and remove the state file;
                    // rewrite it before the direct change, so a kill cannot lose it.
                    if state_written {
                        write_state(&options.state_path, &build_state(options, changes)).map_err(
                            |error| format!("could not rewrite the restore state: {error}"),
                        )?;
                    }
                }
            }
        }
    }
    Ok(options
        .runner
        .capture(&change.apply, CaptureOptions::default())
        .unwrap_or_else(|error| Captured {
            exit_code: 1,
            stdout: String::new(),
            stderr: error,
        }))
}

#[allow(clippy::too_many_arguments)]
fn undo_change(
    options: &SessionOptions,
    guard: &mut Option<Guard>,
    guard_usable: &mut bool,
    change: &PlannedChange,
    changes: &[PlannedChange],
    state_written: bool,
    log: &Log,
) -> Captured {
    if *guard_usable {
        if let Some(running) = guard.as_mut() {
            match running.request(&format!("undo {}", change.guard)) {
                Ok(reply) => return reply,
                Err(error) => {
                    *guard_usable = false;
                    log.line(&format!(
                        "The restore guard stopped ({error}), undoing directly with sudo"
                    ));
                    if state_written {
                        if let Err(error) =
                            write_state(&options.state_path, &build_state(options, changes))
                        {
                            log.line(&format!("Could not rewrite the restore state: {error}"));
                        }
                    }
                }
            }
        }
    }
    options
        .runner
        .capture(&change.undo, CaptureOptions::default())
        .unwrap_or_else(|error| Captured {
            exit_code: 1,
            stdout: String::new(),
            stderr: error,
        })
}

#[allow(clippy::too_many_arguments)]
fn run_job(
    options: &SessionOptions,
    job: &JobSpec,
    number: usize,
    total: usize,
    next_label: &str,
    status: &Option<Status>,
    log: &Log,
    current_job: &Arc<Mutex<Option<Arc<dyn Job>>>>,
    interrupted: &Arc<AtomicU8>,
    samples_path: &Path,
) -> JobResult {
    let started_at = options.clock.now_ms();
    if let Some(status) = status {
        status.set(&format!(
            "Step {number}/{total}: Job {} (elapsed 0s, {}), next: {next_label}",
            job.name,
            estimate_text(job.estimate_minutes)
        ));
    }
    log.line(&format!(
        "[{number}/{total}] Job {}: {} ({})",
        job.name,
        job.command,
        estimate_text(job.estimate_minutes)
    ));

    let argv = job_argv(job);
    let output_log = log.clone();
    let name = job.name.clone();
    let on_output: crate::runner::OutputHandler = Arc::new(move |_stream, line| {
        output_log.line(&format!("[{name}] {line}"));
    });
    let env = [(
        SESSION_SAMPLES_ENV.to_owned(),
        samples_path.display().to_string(),
    )];
    let spawned = options.runner.spawn_job(
        &argv,
        JobOptions {
            cwd: Some(&options.cwd),
            env: &env,
            clear_env: false,
            process_group: true,
            on_output,
        },
    );

    let exit_code = match spawned {
        Ok(job_handle) => {
            let handle: Arc<dyn Job> = Arc::from(job_handle);
            if let Ok(mut slot) = current_job.lock() {
                *slot = Some(Arc::clone(&handle));
            }
            // A signal can arrive between the spawn and the slot being filled.
            if SessionSignal::from_exit_code(interrupted.load(Ordering::SeqCst)).is_some() {
                handle.stop();
            }
            let result = handle.wait();
            if let Ok(mut slot) = current_job.lock() {
                *slot = None;
            }
            match result {
                Ok(captured) => captured.exit_code,
                Err(error) => {
                    log.line(&format!(
                        "[{number}/{total}] Job {} could not be waited for: {error}",
                        job.name
                    ));
                    1
                }
            }
        }
        Err(error) => {
            log.line(&format!(
                "[{number}/{total}] Job {} could not start: {error}",
                job.name
            ));
            1
        }
    };

    let duration_ms = options.clock.now_ms().saturating_sub(started_at);
    let next = if number < total {
        format!("next: {next_label}")
    } else {
        "restore system settings".to_owned()
    };
    log.line(&format!(
        "[{number}/{total}] Job {} exit {exit_code} after {}. {next}",
        job.name,
        format::duration(duration_ms)
    ));
    JobResult {
        job: job.clone(),
        exit_code,
        duration_ms,
    }
}

fn build_state(options: &SessionOptions, changes: &[PlannedChange]) -> RestoreState {
    RestoreState {
        version: state::STATE_VERSION,
        session_id: options.session_id.clone(),
        created_at: time::iso8601_seconds(options.started_at_ms),
        changes: changes.iter().map(RestoreEntry::from).collect(),
    }
}

fn log_manual_commands(changes: &[PlannedChange], log: &Log) {
    log.line("Manual undo commands:");
    for change in changes.iter().rev() {
        log.line(&format!("  {}", change.undo_command));
    }
}

fn summarize_jobs(results: &[JobResult]) -> String {
    if results.is_empty() {
        return "none ran".to_owned();
    }
    results
        .iter()
        .map(|result| {
            let outcome = if result.exit_code == 0 {
                "ok".to_owned()
            } else {
                format!("failed (exit {})", result.exit_code)
            };
            format!(
                "{} {outcome} ({})",
                result.job.name,
                format::duration(result.duration_ms)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn stderr_detail(stderr: &str) -> String {
    let text = stderr.trim();
    if text.is_empty() {
        String::new()
    } else {
        format!(": {text}")
    }
}

#[allow(clippy::too_many_arguments)]
fn send_notification(
    runner: &dyn Runner,
    log: &Log,
    job_results: &[JobResult],
    planned_jobs: usize,
    restored_all: bool,
    change_failures: usize,
    session_error: Option<&str>,
    interrupted: Option<SessionSignal>,
) {
    let completed = job_results.len();
    let succeeded = job_results
        .iter()
        .filter(|result| result.exit_code == 0)
        .count();
    let mut parts = Vec::new();
    if planned_jobs == 0 {
        parts.push("No jobs were planned.".to_owned());
    } else if completed == 0 {
        parts.push(format!("No jobs ran, {planned_jobs} planned."));
    } else if succeeded == planned_jobs {
        parts.push(format!("All {planned_jobs} jobs succeeded."));
    } else {
        let did_not_run = planned_jobs - completed;
        let mut text = format!(
            "{} of {planned_jobs} jobs did not succeed",
            planned_jobs - succeeded
        );
        if did_not_run > 0 {
            text.push_str(&format!(", {did_not_run} did not run."));
        } else {
            text.push('.');
        }
        parts.push(text);
    }
    if let Some(signal) = interrupted {
        parts.push(format!("Interrupted by {}.", signal.label()));
    }
    if change_failures > 0 {
        parts.push(format!(
            "{change_failures} setup {} failed.",
            if change_failures == 1 {
                "command"
            } else {
                "commands"
            }
        ));
    }
    if session_error.is_some() {
        parts.push("The session reported an error.".to_owned());
    }
    parts.push(if restored_all {
        "Everything restored.".to_owned()
    } else {
        "Restore failed, manual commands are in the log.".to_owned()
    });
    let message = parts.join(" ");
    let script = format!(
        "display notification {} with title {}",
        apple_script_string(&message),
        apple_script_string("benchdown session")
    );
    match runner.capture(
        &["osascript".to_owned(), "-e".to_owned(), script],
        CaptureOptions::default(),
    ) {
        Ok(result) if result.succeeded() => {}
        Ok(result) => log.line(&format!("Notification failed (exit {})", result.exit_code)),
        Err(error) => log.line(&format!("Notification failed: {error}")),
    }
}

fn apple_script_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn is_root(runner: &dyn Runner) -> bool {
    runner
        .capture(
            &["id".to_owned(), "-u".to_owned()],
            CaptureOptions::default(),
        )
        .ok()
        .filter(|result| result.succeeded())
        .map(|result| result.stdout.trim() == "0")
        .unwrap_or(false)
}

/// Refreshes the sudo ticket until dropped.
struct SudoKeepalive {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SudoKeepalive {
    fn stop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for SudoKeepalive {
    fn drop(&mut self) {
        self.stop();
    }
}

fn start_sudo_keepalive(runner: Arc<dyn Runner>, interval: Duration, log: Log) -> SudoKeepalive {
    let (stop, stopped) = mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        loop {
            match stopped.recv_timeout(interval) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    let argv = ["sudo", "-n", "-v"].map(str::to_owned);
                    match runner.capture(&argv, CaptureOptions::default()) {
                        Ok(result) if !result.succeeded() => {
                            log.line(&format!("sudo refresh failed (exit {})", result.exit_code));
                        }
                        Err(error) => log.line(&format!("sudo refresh failed: {error}")),
                        Ok(_) => {}
                    }
                }
            }
        }
    });
    SudoKeepalive {
        stop: Some(stop),
        thread: Some(thread),
    }
}

#[cfg(test)]
mod tests;
