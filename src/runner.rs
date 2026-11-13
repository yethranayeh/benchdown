//! Running external commands, so tests can replace the machine with a fake.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

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
    /// Send stdout to /dev/null instead of buffering it, for a command run in a tight loop.
    pub discard_stdout: bool,
}

/// A process that keeps running after it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spawned {
    pub pid: u32,
}

pub trait Runner: Send + Sync {
    fn capture(&self, argv: &[String], options: CaptureOptions<'_>) -> Result<Captured, String>;
    /// Runs with the terminal attached, so sudo can ask for a password.
    fn interactive(&self, argv: &[String]) -> Result<i32, String>;
    fn spawn(&self, argv: &[String]) -> Result<Spawned, String>;
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

/// A runner that records every command and answers from a script, for tests.
#[cfg(test)]
pub struct FakeRunner {
    pub argv: std::sync::Mutex<Vec<Vec<String>>>,
    pub spawned: std::sync::Mutex<Vec<Vec<String>>>,
    pub clock: Option<Arc<crate::clock::FakeClock>>,
    /// How long each captured command takes on the fake clock.
    pub command_ms: u64,
    respond: std::sync::Mutex<Responder>,
    next_pid: std::sync::atomic::AtomicU32,
}

#[cfg(test)]
impl FakeRunner {
    pub fn new(respond: impl FnMut(&[String]) -> Captured + Send + 'static) -> Self {
        Self {
            argv: std::sync::Mutex::new(Vec::new()),
            spawned: std::sync::Mutex::new(Vec::new()),
            clock: None,
            command_ms: 0,
            respond: std::sync::Mutex::new(Box::new(respond)),
            next_pid: std::sync::atomic::AtomicU32::new(4_000),
        }
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
}

#[cfg(test)]
impl Runner for FakeRunner {
    fn capture(&self, argv: &[String], _options: CaptureOptions<'_>) -> Result<Captured, String> {
        self.argv
            .lock()
            .expect("the record is not poisoned")
            .push(argv.to_vec());
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
        Ok(0)
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
}
