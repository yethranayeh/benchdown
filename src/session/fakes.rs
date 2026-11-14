//! Test doubles for the session: a scripted runner, its guard and its jobs.
//!
//! Nothing here starts a real process. The guard's own script is tested separately
//! with fake commands on a temporary `PATH`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::clock::FakeClock;
use crate::runner::{
    CaptureOptions, Captured, Job, JobOptions, OutputStream, PipeProcess, Runner, Spawned,
};
use crate::session::{SessionSignal, SignalSource};

/// What one call was, in the order the session made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeCall {
    pub method: &'static str,
    pub argv: Vec<String>,
}

/// One job as the session started it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedJob {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub clear_env: bool,
    pub process_group: bool,
}

/// A job that finishes at once, or waits until it is stopped.
pub struct FakeJob {
    result: Mutex<Option<Result<Captured, String>>>,
    finished: Condvar,
    stopped: AtomicBool,
}

impl FakeJob {
    fn finished(_pid: u32, exit_code: i32) -> Self {
        Self {
            result: Mutex::new(Some(Ok(Captured {
                exit_code,
                stdout: String::new(),
                stderr: String::new(),
            }))),
            finished: Condvar::new(),
            stopped: AtomicBool::new(false),
        }
    }

    fn hanging(_pid: u32) -> Self {
        Self {
            result: Mutex::new(None),
            finished: Condvar::new(),
            stopped: AtomicBool::new(false),
        }
    }

    pub fn was_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    fn finish(&self, exit_code: i32) {
        let mut result = self.result.lock().expect("the job is not poisoned");
        if result.is_none() {
            *result = Some(Ok(Captured {
                exit_code,
                stdout: String::new(),
                stderr: String::new(),
            }));
            self.finished.notify_all();
        }
    }
}

impl Job for FakeJob {
    fn wait(&self) -> Result<Captured, String> {
        let mut result = self.result.lock().expect("the job is not poisoned");
        while result.is_none() {
            result = self.finished.wait(result).expect("the job is not poisoned");
        }
        result.take().expect("the job has a result")
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.finish(143);
    }
}

/// The guard side of a session, with scripted replies.
pub struct FakeGuard {
    pub requests: Mutex<Vec<String>>,
    pub closed: AtomicBool,
    pub die_on: Mutex<HashSet<String>>,
    pub failures: Mutex<HashMap<String, Captured>>,
    replies: Mutex<VecDeque<String>>,
    exit_code: i32,
}

impl FakeGuard {
    fn new(_start_line: String) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            die_on: Mutex::new(HashSet::new()),
            failures: Mutex::new(HashMap::new()),
            replies: Mutex::new(VecDeque::new()),
            exit_code: 0,
        }
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("the guard is not poisoned")
            .clone()
    }
}

impl FakeGuard {
    pub fn write_line(&self, line: &str) -> Result<(), String> {
        self.requests
            .lock()
            .expect("the guard is not poisoned")
            .push(line.to_owned());
        if line == "END" {
            self.replies
                .lock()
                .expect("the guard is not poisoned")
                .push_back("0 ".to_owned());
            return Ok(());
        }
        if self
            .die_on
            .lock()
            .expect("the guard is not poisoned")
            .contains(line)
        {
            return Ok(());
        }
        let reply = match self
            .failures
            .lock()
            .expect("the guard is not poisoned")
            .get(line)
        {
            Some(failure) => format!("{} {}", failure.exit_code, failure.stderr),
            None => {
                if line.starts_with("apply ") || line.starts_with("undo ") {
                    "0 ".to_owned()
                } else {
                    "2 rejected request".to_owned()
                }
            }
        };
        self.replies
            .lock()
            .expect("the guard is not poisoned")
            .push_back(reply);
        Ok(())
    }

    pub fn read_line(&self) -> Result<String, String> {
        if self
            .requests
            .lock()
            .expect("the guard is not poisoned")
            .last()
            .is_some_and(|last| {
                self.die_on
                    .lock()
                    .expect("the guard is not poisoned")
                    .contains(last)
            })
        {
            return Err("the guard died".to_owned());
        }
        self.replies
            .lock()
            .expect("the guard is not poisoned")
            .pop_front()
            .ok_or_else(|| "the guard has no reply".to_owned())
    }

    pub fn close(&self) -> Result<Captured, String> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(Captured {
            exit_code: self.exit_code,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

/// A runner that answers captures from a script and records everything else.
pub type FakeResponder = Arc<dyn Fn(&[String]) -> Captured + Send + Sync>;

pub struct FakeRunner {
    pub job_outputs: Mutex<HashMap<String, Vec<String>>>,
    pub calls: Mutex<Vec<FakeCall>>,
    pub captures: Mutex<Vec<Vec<String>>>,
    pub jobs: Mutex<Vec<RecordedJob>>,
    pub spawns: Mutex<Vec<Vec<String>>>,
    pub interactives: Mutex<Vec<Vec<String>>>,
    pub jobs_started: Mutex<Vec<Arc<FakeJob>>>,
    pub guard: Mutex<Option<Arc<FakeGuard>>>,
    respond: FakeResponder,
    job_exit_codes: Mutex<HashMap<String, i32>>,
    hanging: Mutex<HashSet<String>>,
    guard_start_line: String,
    guard_die_on: Mutex<HashSet<String>>,
    guard_failures: Mutex<HashMap<String, Captured>>,
    interactive_exit: Mutex<i32>,
    next_pid: AtomicU32,
}

impl FakeRunner {
    pub fn new(
        clock: Arc<FakeClock>,
        respond: impl Fn(&[String]) -> Captured + Send + Sync + 'static,
    ) -> Self {
        let _ = clock;
        Self {
            job_outputs: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
            captures: Mutex::new(Vec::new()),
            jobs: Mutex::new(Vec::new()),
            spawns: Mutex::new(Vec::new()),
            interactives: Mutex::new(Vec::new()),
            jobs_started: Mutex::new(Vec::new()),
            guard: Mutex::new(None),
            respond: Arc::new(respond),
            job_exit_codes: Mutex::new(HashMap::new()),
            hanging: Mutex::new(HashSet::new()),
            guard_start_line: "ready".to_owned(),
            guard_die_on: Mutex::new(HashSet::new()),
            guard_failures: Mutex::new(HashMap::new()),
            interactive_exit: Mutex::new(0),
            next_pid: AtomicU32::new(4_000),
        }
    }

    /// A machine with everything off and no Wi-Fi, so a session changes nothing.
    pub fn quiet(clock: Arc<FakeClock>) -> Self {
        Self::new(clock, |argv| match argv.join(" ").as_str() {
            "mdutil -s -a" => text("/System/Volumes/Data:\n\tIndexing disabled. \n"),
            "tmutil destinationinfo -X" => text(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n</dict>\n</plist>\n",
            ),
            value if value.starts_with("networksetup -getairportpower") => {
                text("Wi-Fi Power (en0): Off\n")
            }
            _ => ok(),
        })
    }

    pub fn guard(&self) -> Option<Arc<FakeGuard>> {
        self.guard
            .lock()
            .expect("the guard is not poisoned")
            .clone()
    }

    pub fn set_job_exit(&self, command: &str, exit_code: i32) {
        self.job_exit_codes
            .lock()
            .expect("the jobs are not poisoned")
            .insert(command.to_owned(), exit_code);
    }

    pub fn set_job_output(&self, command: &str, lines: &[&str]) {
        self.job_outputs
            .lock()
            .expect("the jobs are not poisoned")
            .insert(
                command.to_owned(),
                lines.iter().map(|line| (*line).to_owned()).collect(),
            );
    }

    pub fn die_on_guard_request(&self, request: &str) {
        self.guard_die_on
            .lock()
            .expect("the guard is not poisoned")
            .insert(request.to_owned());
    }

    pub fn fail_guard_request(&self, request: &str, exit_code: i32, stderr: &str) {
        self.guard_failures
            .lock()
            .expect("the guard is not poisoned")
            .insert(
                request.to_owned(),
                Captured {
                    exit_code,
                    stdout: String::new(),
                    stderr: stderr.to_owned(),
                },
            );
    }

    pub fn jobs(&self) -> Vec<RecordedJob> {
        self.jobs.lock().expect("the jobs are not poisoned").clone()
    }

    pub fn jobs_started(&self) -> Vec<Arc<FakeJob>> {
        self.jobs_started
            .lock()
            .expect("the jobs are not poisoned")
            .clone()
    }

    pub fn hang(&self, command: &str) {
        self.hanging
            .lock()
            .expect("the jobs are not poisoned")
            .insert(command.to_owned());
    }

    pub fn set_guard_start_line(&mut self, line: &str) {
        self.guard_start_line = line.to_owned();
    }

    pub fn calls(&self) -> Vec<FakeCall> {
        self.calls
            .lock()
            .expect("the calls are not poisoned")
            .clone()
    }

    fn record(&self, method: &'static str, argv: &[String]) {
        self.calls
            .lock()
            .expect("the calls are not poisoned")
            .push(FakeCall {
                method,
                argv: argv.to_vec(),
            });
    }
}

impl Runner for FakeRunner {
    fn capture(&self, argv: &[String], _options: CaptureOptions<'_>) -> Result<Captured, String> {
        self.record("capture", argv);
        self.captures
            .lock()
            .expect("the captures are not poisoned")
            .push(argv.to_vec());
        Ok((self.respond)(argv))
    }

    fn interactive(&self, argv: &[String]) -> Result<i32, String> {
        self.record("interactive", argv);
        self.interactives
            .lock()
            .expect("the interviews are not poisoned")
            .push(argv.to_vec());
        Ok(*self
            .interactive_exit
            .lock()
            .expect("the interactive exit code is not poisoned"))
    }

    fn spawn(&self, argv: &[String]) -> Result<Spawned, String> {
        self.record("spawn", argv);
        self.spawns
            .lock()
            .expect("the spawns are not poisoned")
            .push(argv.to_vec());
        Ok(Spawned { pid: 6_666 })
    }

    fn spawn_job(&self, argv: &[String], options: JobOptions<'_>) -> Result<Box<dyn Job>, String> {
        self.record("spawn_job", argv);
        let key = argv.join(" ");
        self.jobs
            .lock()
            .expect("the jobs are not poisoned")
            .push(RecordedJob {
                argv: argv.to_vec(),
                cwd: options.cwd.map(Path::to_path_buf),
                env: options.env.to_vec(),
                clear_env: options.clear_env,
                process_group: options.process_group,
            });
        let pid = self.next_pid.fetch_add(1, Ordering::SeqCst);
        let hanging = self
            .hanging
            .lock()
            .expect("the jobs are not poisoned")
            .contains(&key);
        let exit_code = *self
            .job_exit_codes
            .lock()
            .expect("the jobs are not poisoned")
            .get(&key)
            .unwrap_or(&0);
        if let Some(lines) = self
            .job_outputs
            .lock()
            .expect("the jobs are not poisoned")
            .get(&key)
        {
            for line in lines {
                (options.on_output)(OutputStream::Stdout, line);
            }
        }
        let job: Arc<FakeJob> = if hanging {
            Arc::new(FakeJob::hanging(pid))
        } else {
            Arc::new(FakeJob::finished(pid, exit_code))
        };
        self.jobs_started
            .lock()
            .expect("the jobs are not poisoned")
            .push(Arc::clone(&job));
        Ok(Box::new(SharedJob(job)))
    }

    fn pipe(&self, argv: &[String], _cwd: Option<&Path>) -> Result<Box<dyn PipeProcess>, String> {
        self.record("pipe", argv);
        let guard = Arc::new(FakeGuard::new(self.guard_start_line.clone()));
        guard
            .replies
            .lock()
            .expect("the guard is not poisoned")
            .push_back(self.guard_start_line.clone());
        *guard.die_on.lock().expect("the guard is not poisoned") = self
            .guard_die_on
            .lock()
            .expect("the guard is not poisoned")
            .clone();
        *guard.failures.lock().expect("the guard is not poisoned") = self
            .guard_failures
            .lock()
            .expect("the guard is not poisoned")
            .clone();
        *self.guard.lock().expect("the guard is not poisoned") = Some(Arc::clone(&guard));
        Ok(Box::new(SharedGuard(guard)))
    }
}

/// Lets a test keep a handle to the job the session started.
struct SharedJob(Arc<FakeJob>);

impl Job for SharedJob {
    fn wait(&self) -> Result<Captured, String> {
        self.0.wait()
    }

    fn stop(&self) {
        self.0.stop()
    }
}

/// Lets a test keep a handle to the guard the session started.
struct SharedGuard(Arc<FakeGuard>);

impl PipeProcess for SharedGuard {
    fn write_line(&mut self, line: &str) -> Result<(), String> {
        self.0.write_line(line)
    }

    fn read_line(&mut self) -> Result<String, String> {
        self.0.read_line()
    }

    fn close(&mut self) -> Result<Captured, String> {
        self.0.close()
    }
}

pub fn ok() -> Captured {
    Captured {
        exit_code: 0,
        stdout: String::new(),
        stderr: String::new(),
    }
}

pub fn text(output: &str) -> Captured {
    Captured {
        exit_code: 0,
        stdout: output.to_owned(),
        stderr: String::new(),
    }
}

/// Signals a test can emit by hand.
pub type FakeSignalHandler = Box<dyn Fn(SessionSignal) + Send + Sync>;

pub struct FakeSignals {
    handlers: Mutex<Vec<FakeSignalHandler>>,
}

impl FakeSignals {
    pub fn new() -> Self {
        Self {
            handlers: Mutex::new(Vec::new()),
        }
    }

    pub fn emit(&self, signal: SessionSignal) {
        let handlers = self.handlers.lock().expect("the signals are not poisoned");
        for handler in handlers.iter() {
            handler(signal);
        }
    }
}

impl Default for FakeSignals {
    fn default() -> Self {
        Self::new()
    }
}

impl SignalSource for FakeSignals {
    fn on(&self, handler: FakeSignalHandler) {
        self.handlers
            .lock()
            .expect("the signals are not poisoned")
            .push(handler);
    }
}
