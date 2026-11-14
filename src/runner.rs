//! Running external commands, so tests can replace the machine with a fake.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::clock::{Clock, SystemClock};

/// What a finished command reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Captured {
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0
    }
}

/// How to run one command.
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureOptions<'a> {
    pub cwd: Option<&'a Path>,
    /// Variables added to the command's environment, in order.
    pub env: &'a [(String, String)],
    /// Start the command with an empty environment, then add `env`.
    pub clear_env: bool,
    /// Send stdout to /dev/null instead of buffering it, for a command run in a tight loop.
    pub discard_stdout: bool,
}

/// A process that keeps running after it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spawned {
    pub pid: u32,
}

/// Which stream one output line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Called once per complete line of a job's output, from reader threads.
pub type OutputHandler = Arc<dyn Fn(OutputStream, &str) + Send + Sync>;

/// How to run one job: its own process group, an added environment, live output.
pub struct JobOptions<'a> {
    pub cwd: Option<&'a Path>,
    /// Variables added to the inherited environment.
    pub env: &'a [(String, String)],
    pub clear_env: bool,
    /// Starts the job in its own process group in the same session.
    pub process_group: bool,
    /// Called once per complete line of output, from reader threads.
    pub on_output: OutputHandler,
}

/// A running job the session can wait for or stop.
pub trait Job: Send + Sync {
    /// Waits for the job and returns its exit code; output was streamed.
    fn wait(&self) -> Result<Captured, String>;
    /// Sends SIGTERM to the job's process group, then SIGKILL after a grace period.
    fn stop(&self);
}

/// A running command with line-based stdin and stdout, for the restore guard.
pub trait PipeProcess: Send {
    fn write_line(&mut self, line: &str) -> Result<(), String>;
    fn read_line(&mut self) -> Result<String, String>;
    fn close(&mut self) -> Result<Captured, String>;
}

pub trait Runner: Send + Sync {
    fn capture(&self, argv: &[String], options: CaptureOptions<'_>) -> Result<Captured, String>;
    /// Runs with the terminal attached, so sudo can ask for a password.
    fn interactive(&self, argv: &[String]) -> Result<i32, String>;
    fn spawn(&self, argv: &[String]) -> Result<Spawned, String>;
    fn spawn_job(&self, argv: &[String], options: JobOptions<'_>) -> Result<Box<dyn Job>, String>;
    fn pipe(&self, argv: &[String], cwd: Option<&Path>) -> Result<Box<dyn PipeProcess>, String>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn capture(&self, argv: &[String], options: CaptureOptions<'_>) -> Result<Captured, String> {
        let (program, arguments) = split(argv)?;
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(if options.discard_stdout {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stderr(Stdio::piped());
        if let Some(directory) = options.cwd {
            command.current_dir(directory);
        }
        if options.clear_env {
            command.env_clear();
        }
        command.envs(options.env.iter().map(|(key, value)| (key, value)));

        let output = command
            .output()
            .map_err(|error| format!("{}: {error}", argv.join(" ")))?;
        Ok(Captured {
            exit_code: exit_code(&output.status),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn interactive(&self, argv: &[String]) -> Result<i32, String> {
        let (program, arguments) = split(argv)?;
        let status = Command::new(program)
            .args(arguments)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| format!("{}: {error}", argv.join(" ")))?;
        Ok(exit_code(&status))
    }

    fn spawn(&self, argv: &[String]) -> Result<Spawned, String> {
        let (program, arguments) = split(argv)?;
        let child = Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("{}: {error}", argv.join(" ")))?;
        Ok(Spawned { pid: child.id() })
    }

    fn spawn_job(&self, argv: &[String], options: JobOptions<'_>) -> Result<Box<dyn Job>, String> {
        let (program, arguments) = split(argv)?;
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(directory) = options.cwd {
            command.current_dir(directory);
        }
        if options.clear_env {
            command.env_clear();
        }
        command.envs(options.env.iter().map(|(key, value)| (key, value)));
        #[cfg(unix)]
        if options.process_group {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command
            .spawn()
            .map_err(|error| format!("{}: {error}", argv.join(" ")))?;
        let pid = child.id();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "the job has no stdout pipe".to_owned())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "the job has no stderr pipe".to_owned())?;
        let on_output = Arc::clone(&options.on_output);
        let readers = vec![
            pump(stdout, OutputStream::Stdout, Arc::clone(&on_output)),
            pump(stderr, OutputStream::Stderr, on_output),
        ];

        Ok(Box::new(SystemJob {
            pid,
            child: Mutex::new(child),
            readers: Mutex::new(readers),
            exit: Arc::new(Mutex::new(None)),
            stop_requested: AtomicBool::new(false),
        }))
    }

    fn pipe(&self, argv: &[String], cwd: Option<&Path>) -> Result<Box<dyn PipeProcess>, String> {
        let (program, arguments) = split(argv)?;
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(directory) = cwd {
            command.current_dir(directory);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("{}: {error}", argv.join(" ")))?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "the process has no stdout pipe".to_owned())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "the process has no stderr pipe".to_owned())?;

        let collected = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&collected);
        let stderr_thread = std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buffer = Vec::new();
            while reader.read_until(b'\n', &mut buffer).unwrap_or(0) > 0 {
                if let Ok(mut text) = sink.lock() {
                    text.push_str(&String::from_utf8_lossy(&buffer));
                }
                buffer.clear();
            }
        });

        Ok(Box::new(SystemPipe {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr: collected,
            stderr_thread: Some(stderr_thread),
            result: None,
        }))
    }
}

/// A job started by `spawn_job`, with its reader threads.
struct SystemJob {
    pid: u32,
    child: Mutex<Child>,
    readers: Mutex<Vec<JoinHandle<()>>>,
    exit: Arc<Mutex<Option<i32>>>,
    stop_requested: AtomicBool,
}

impl Job for SystemJob {
    fn wait(&self) -> Result<Captured, String> {
        let status = {
            let mut child = self
                .child
                .lock()
                .map_err(|_| "the job's process handle is poisoned".to_owned())?;
            child
                .wait()
                .map_err(|error| format!("could not wait for the job: {error}"))?
        };
        let code = exit_code(&status);
        if let Ok(mut exit) = self.exit.lock() {
            *exit = Some(code);
        }
        if let Ok(mut readers) = self.readers.lock() {
            for reader in readers.drain(..) {
                let _ = reader.join();
            }
        }
        Ok(Captured {
            exit_code: code,
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    fn stop(&self) {
        if self.stop_requested.swap(true, Ordering::SeqCst) {
            return;
        }
        signal_process_group(self.pid, "TERM");
        let pid = self.pid;
        let exit = Arc::clone(&self.exit);
        std::thread::spawn(move || {
            for _ in 0..STOP_GRACE_STEPS {
                std::thread::sleep(STOP_GRACE_STEP);
                let finished = exit.lock().map(|exit| exit.is_some()).unwrap_or(true);
                if finished {
                    return;
                }
            }
            signal_process_group(pid, "KILL");
        });
    }
}

/// A command with piped stdin and stdout, used for the restore guard.
struct SystemPipe {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: Arc<Mutex<String>>,
    stderr_thread: Option<JoinHandle<()>>,
    result: Option<Captured>,
}

impl PipeProcess for SystemPipe {
    fn write_line(&mut self, line: &str) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "the process's input is closed".to_owned())?;
        writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .map_err(|error| format!("could not write to the process: {error}"))
    }

    fn read_line(&mut self) -> Result<String, String> {
        let mut buffer = Vec::new();
        let read = self
            .stdout
            .read_until(b'\n', &mut buffer)
            .map_err(|error| format!("could not read from the process: {error}"))?;
        if read == 0 {
            return Err("the process closed its output".to_owned());
        }
        let text = String::from_utf8_lossy(&buffer);
        Ok(text.trim_end_matches(['\n', '\r']).to_owned())
    }

    fn close(&mut self) -> Result<Captured, String> {
        if let Some(result) = &self.result {
            return Ok(result.clone());
        }
        self.stdin.take();
        let status = self
            .child
            .wait()
            .map_err(|error| format!("could not wait for the process: {error}"))?;
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
        let stderr = self
            .stderr
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default();
        let result = Captured {
            exit_code: exit_code(&status),
            stdout: String::new(),
            stderr,
        };
        self.result = Some(result.clone());
        Ok(result)
    }
}

impl Drop for SystemPipe {
    fn drop(&mut self) {
        // Closing stdin tells the guard the wrapper is gone, so it restores and exits.
        self.stdin.take();
        let _ = self.close();
    }
}

/// How long a stopped job is given before SIGKILL.
const STOP_GRACE_STEP: std::time::Duration = std::time::Duration::from_millis(100);
const STOP_GRACE_STEPS: u32 = 50;

/// Sends a signal to a job's process group, without linking to libc.
fn signal_process_group(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([format!("-{signal}"), format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Emits complete lines from a job's stream, including a final line without a newline.
fn pump<R: Read + Send + 'static>(
    reader: R,
    stream: OutputStream,
    on_output: OutputHandler,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&buffer);
                    on_output(stream, text.trim_end_matches(['\n', '\r']));
                }
                Err(_) => break,
            }
        }
    })
}

fn split(argv: &[String]) -> Result<(&str, &[String]), String> {
    match argv.split_first() {
        Some((program, arguments)) => Ok((program, arguments)),
        None => Err("an empty command line cannot run".to_owned()),
    }
}

#[cfg(unix)]
fn exit_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;

    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

#[cfg(not(unix))]
fn exit_code(status: &std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// The runner and clock one run uses.
#[derive(Clone)]
pub struct System {
    pub runner: Arc<dyn Runner>,
    pub clock: Arc<dyn Clock>,
}

impl System {
    pub fn new() -> Self {
        Self {
            runner: Arc::new(SystemRunner),
            clock: Arc::new(SystemClock),
        }
    }
}

impl Default for System {
    fn default() -> Self {
        Self::new()
    }
}

/// A temporary path beside the system's temporary directory.
pub fn temporary_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("benchdown-{}-{name}", std::process::id()))
}

#[cfg(test)]
type Responder = Box<dyn FnMut(&[String]) -> Captured + Send>;

/// What one captured command was run with, owned so a test can assert on it.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCapture {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub clear_env: bool,
    pub discard_stdout: bool,
}

/// A runner that records every command and answers from a script, for tests.
#[cfg(test)]
pub struct FakeRunner {
    pub argv: std::sync::Mutex<Vec<Vec<String>>>,
    pub captures: std::sync::Mutex<Vec<RecordedCapture>>,
    pub spawned: std::sync::Mutex<Vec<Vec<String>>>,
    pub clock: Option<Arc<crate::clock::FakeClock>>,
    /// How long each captured command takes on the fake clock.
    pub command_ms: u64,
    respond: std::sync::Mutex<Responder>,
    next_pid: std::sync::atomic::AtomicU32,
    interactive_exit: std::sync::atomic::AtomicI32,
}

#[cfg(test)]
impl FakeRunner {
    pub fn new(respond: impl FnMut(&[String]) -> Captured + Send + 'static) -> Self {
        Self {
            argv: std::sync::Mutex::new(Vec::new()),
            captures: std::sync::Mutex::new(Vec::new()),
            spawned: std::sync::Mutex::new(Vec::new()),
            clock: None,
            command_ms: 0,
            respond: std::sync::Mutex::new(Box::new(respond)),
            next_pid: std::sync::atomic::AtomicU32::new(4_000),
            interactive_exit: std::sync::atomic::AtomicI32::new(0),
        }
    }

    /// Sets the exit code `interactive` reports, so a refused password can be tested.
    pub fn with_interactive_exit(self, exit_code: i32) -> Self {
        self.interactive_exit
            .store(exit_code, std::sync::atomic::Ordering::SeqCst);
        self
    }

    pub fn always(exit_code: i32) -> Self {
        Self::new(move |_| Captured {
            exit_code,
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    pub fn with_clock(mut self, clock: Arc<crate::clock::FakeClock>, command_ms: u64) -> Self {
        self.clock = Some(clock);
        self.command_ms = command_ms;
        self
    }

    pub fn argv(&self) -> Vec<Vec<String>> {
        self.argv
            .lock()
            .expect("the record is not poisoned")
            .clone()
    }

    pub fn captures(&self) -> Vec<RecordedCapture> {
        self.captures
            .lock()
            .expect("the record is not poisoned")
            .clone()
    }
}

#[cfg(test)]
impl Runner for FakeRunner {
    fn capture(&self, argv: &[String], options: CaptureOptions<'_>) -> Result<Captured, String> {
        self.argv
            .lock()
            .expect("the record is not poisoned")
            .push(argv.to_vec());
        self.captures
            .lock()
            .expect("the record is not poisoned")
            .push(RecordedCapture {
                argv: argv.to_vec(),
                cwd: options.cwd.map(Path::to_path_buf),
                env: options.env.to_vec(),
                clear_env: options.clear_env,
                discard_stdout: options.discard_stdout,
            });
        if let Some(clock) = &self.clock {
            clock.advance(self.command_ms);
        }
        let mut respond = self.respond.lock().expect("the script is not poisoned");
        Ok(respond(argv))
    }

    fn interactive(&self, argv: &[String]) -> Result<i32, String> {
        self.argv
            .lock()
            .expect("the record is not poisoned")
            .push(argv.to_vec());
        Ok(self
            .interactive_exit
            .load(std::sync::atomic::Ordering::SeqCst))
    }

    fn spawn(&self, argv: &[String]) -> Result<Spawned, String> {
        self.spawned
            .lock()
            .expect("the record is not poisoned")
            .push(argv.to_vec());
        let pid = self
            .next_pid
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Spawned { pid })
    }

    fn spawn_job(
        &self,
        _argv: &[String],
        _options: JobOptions<'_>,
    ) -> Result<Box<dyn Job>, String> {
        Err("this fake runner does not start jobs".to_owned())
    }

    fn pipe(&self, _argv: &[String], _cwd: Option<&Path>) -> Result<Box<dyn PipeProcess>, String> {
        Err("this fake runner does not start pipes".to_owned())
    }
}
