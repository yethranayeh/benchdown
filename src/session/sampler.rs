//! Sampling load averages and the busiest processes while a session runs.
//!
//! The file has one header row and one row per process, so a single sample reads
//! across several lines that share its `time` column. `parse_samples_file` groups
//! those rows back into one entry per sample time. Timestamps are ISO 8601 UTC
//! seconds (`2026-11-08T10:00:05Z`), which keeps the file readable and parses back
//! to the Unix seconds the block log uses.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use crate::clock::Clock;
use crate::runner::{CaptureOptions, Runner};
use crate::session::time;

pub const SAMPLES_FILE_NAME: &str = "samples.tsv";

/// Default seconds between two samples.
pub const DEFAULT_SAMPLE_SECONDS: u64 = 5;

/// Tells each job where the running session writes its samples.
pub const SESSION_SAMPLES_ENV: &str = "BENCHDOWN_SESSION_SAMPLES";

/// Processes kept per sample, busiest first.
const PROCESSES_PER_SAMPLE: usize = 10;

const LOAD_COMMAND: [&str; 3] = ["sysctl", "-n", "vm.loadavg"];

/// `ps` CPU on macOS is a decaying average, so it shows who has been busy
/// rather than an instant reading.
const PROCESS_COMMAND: [&str; 5] = ["ps", "-A", "-r", "-o", "pcpu=,rss=,pid=,comm="];

/// The columns of a samples file, in order.
pub const SAMPLES_HEADER: [&str; 8] = [
    "time",
    "load1",
    "load5",
    "load15",
    "cpu_percent",
    "rss_kb",
    "pid",
    "command",
];

/// One process from one sample.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionProcess {
    pub cpu_percent: f64,
    pub command: String,
}

/// One load and process sample, with every process row for the same time.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSample {
    /// Unix seconds when the sample was taken.
    pub seconds: f64,
    pub load1: f64,
    pub processes: Vec<SessionProcess>,
}

/// A sampler that runs in the background until stopped.
pub trait ActiveSampler: Send {
    fn stop(&mut self);
}

/// What the sampler needs to run.
pub struct SamplerOptions {
    pub runner: Arc<dyn Runner>,
    pub clock: Arc<dyn Clock>,
    pub path: PathBuf,
    pub interval_ms: u64,
    /// Called once, for the first failed sample.
    pub on_first_failure: Arc<dyn Fn(String) + Send + Sync>,
}

/// Writes the header, takes the first sample, then samples in the background.
pub fn start_sampler(options: SamplerOptions) -> Result<Box<dyn ActiveSampler>, String> {
    if let Some(parent) = options.path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let header = format!("{}\n", SAMPLES_HEADER.join("\t"));
    std::fs::write(&options.path, header)
        .map_err(|error| format!("{}: {error}", options.path.display()))?;

    let reported = Arc::new(AtomicBool::new(false));
    take_sample(&options, &reported);

    let (stop, stopped) = mpsc::channel::<()>();
    let path = options.path.clone();
    let runner = Arc::clone(&options.runner);
    let clock = Arc::clone(&options.clock);
    let on_first_failure = Arc::clone(&options.on_first_failure);
    let interval = Duration::from_millis(options.interval_ms.max(1));
    let thread = std::thread::spawn(move || {
        loop {
            match stopped.recv_timeout(interval) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    let inner = SamplerOptions {
                        runner: Arc::clone(&runner),
                        clock: Arc::clone(&clock),
                        path: path.clone(),
                        interval_ms: 0,
                        on_first_failure: Arc::clone(&on_first_failure),
                    };
                    take_sample(&inner, &reported);
                }
            }
        }
    });

    Ok(Box::new(RunningSampler {
        stop: Some(stop),
        thread: Some(thread),
    }))
}

struct RunningSampler {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ActiveSampler for RunningSampler {
    fn stop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RunningSampler {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Takes one sample and appends its process rows, reporting only the first failure.
fn take_sample(options: &SamplerOptions, reported: &AtomicBool) {
    match sample_once(
        options.runner.as_ref(),
        options.clock.as_ref(),
        &options.path,
    ) {
        Ok(()) => {}
        Err(message) => {
            if !reported.swap(true, Ordering::SeqCst) {
                (options.on_first_failure)(message);
            }
        }
    }
}

/// Takes one sample and appends it to the file. Public so tests can drive it.
pub fn sample_once(runner: &dyn Runner, clock: &dyn Clock, path: &Path) -> Result<(), String> {
    let timestamp = time::iso8601_seconds(clock.now_ms());
    let load = runner.capture(&LOAD_COMMAND.map(str::to_owned), CaptureOptions::default())?;
    let processes = runner.capture(
        &PROCESS_COMMAND.map(str::to_owned),
        CaptureOptions::default(),
    )?;
    if !load.succeeded() || !processes.succeeded() {
        return Err(format!(
            "sysctl exit {}, ps exit {}",
            load.exit_code, processes.exit_code
        ));
    }

    let loads = parse_load_averages(&load.stdout);
    let rows: Vec<String> = parse_processes(&processes.stdout)
        .into_iter()
        .take(PROCESSES_PER_SAMPLE)
        .map(|row| {
            format!(
                "{timestamp}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                loads[0], loads[1], loads[2], row.cpu, row.rss_kb, row.pid, row.command
            )
        })
        .collect();
    if rows.is_empty() {
        return Ok(());
    }

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    writeln!(file, "{}", rows.join("\n")).map_err(|error| format!("{}: {error}", path.display()))
}

/// Parses `sysctl -n vm.loadavg` output such as `{ 1.20 1.05 0.98 }`.
pub fn parse_load_averages(output: &str) -> [String; 3] {
    let mut numbers = output
        .split(|character: char| !(character.is_ascii_digit() || character == '.'))
        .filter(|part| !part.is_empty());
    let mut load = [String::new(), String::new(), String::new()];
    for slot in &mut load {
        if let Some(number) = numbers.next() {
            *slot = number.to_owned();
        }
    }
    load
}

/// One row from `ps -A -r -o pcpu=,rss=,pid=,comm=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRow {
    pub cpu: String,
    pub rss_kb: String,
    pub pid: String,
    pub command: String,
}

/// Parses `ps` output, keeping a command with spaces whole.
pub fn parse_processes(output: &str) -> Vec<ProcessRow> {
    let mut rows = Vec::new();
    for line in output.lines() {
        let mut rest = line.trim();
        if rest.is_empty() {
            continue;
        }
        let mut fields = ["", "", ""];
        let mut complete = true;
        for field in &mut fields {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            if end == 0 {
                complete = false;
                break;
            }
            *field = &rest[..end];
            rest = rest[end..].trim_start();
        }
        if !complete || rest.is_empty() {
            continue;
        }
        rows.push(ProcessRow {
            cpu: fields[0].to_owned(),
            rss_kb: fields[1].to_owned(),
            pid: fields[2].to_owned(),
            command: rest.to_owned(),
        });
    }
    rows
}

/// Reads a samples file back into one entry per sample time, in file order.
pub fn parse_samples_file(text: &str) -> Result<Vec<SessionSample>, String> {
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return Err("The samples file is empty".to_owned());
    };
    let expected = SAMPLES_HEADER.join("\t");
    if header != expected {
        return Err(format!("The samples file header must be {expected}"));
    }

    let mut samples: Vec<SessionSample> = Vec::new();
    for line in lines {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != SAMPLES_HEADER.len() {
            continue;
        }
        let Some(seconds) = time::parse_iso8601_seconds(fields[0]) else {
            continue;
        };
        let Ok(load1) = fields[1].parse::<f64>() else {
            continue;
        };
        let process = match fields[4].parse::<f64>() {
            Ok(cpu_percent) => Some(SessionProcess {
                cpu_percent,
                command: fields[7].to_owned(),
            }),
            Err(_) => None,
        };

        match samples.last_mut() {
            Some(last) if last.seconds == seconds => {
                if let Some(process) = process {
                    last.processes.push(process);
                }
            }
            _ => samples.push(SessionSample {
                seconds,
                load1,
                processes: process.into_iter().collect(),
            }),
        }
    }
    Ok(samples)
}

/// Reads and parses a samples file, naming it in any error.
pub fn read_samples_file(path: &Path) -> Result<Vec<SessionSample>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    parse_samples_file(&text).map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use crate::runner::{Captured, FakeRunner};

    fn temporary(name: &str) -> PathBuf {
        use std::sync::atomic::AtomicU64;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "benchdown-sampler-{}-{serial}-{name}",
            std::process::id()
        ))
    }

    fn scripted_runner(clock: Arc<FakeClock>) -> Arc<FakeRunner> {
        Arc::new(
            FakeRunner::new(|argv| match argv.first().map(String::as_str) {
                Some("sysctl") => Captured {
                    exit_code: 0,
                    stdout: "{ 2.50 2.00 1.50 }\n".to_owned(),
                    stderr: String::new(),
                },
                Some("ps") => Captured {
                    exit_code: 0,
                    stdout: " 42.0 51200   101 /Applications/Busy App.app/Contents/MacOS/Busy App\n  1.5  2048   202 /usr/sbin/idle\n".to_owned(),
                    stderr: String::new(),
                },
                _ => Captured {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            })
            .with_clock(clock, 0),
        )
    }

    #[test]
    fn load_averages_are_read_from_sysctl_output() {
        assert_eq!(
            parse_load_averages("{ 13.75 13.19 13.05 }\n"),
            ["13.75", "13.19", "13.05"]
        );
    }

    #[test]
    fn process_rows_keep_commands_with_spaces_whole() {
        let rows = parse_processes(
            " 61.7 574864 10131 /Applications/Visual Studio Code.app/Contents/MacOS/Electron\n\n  0.0   16     1 /sbin/launchd\n",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].cpu, "61.7");
        assert_eq!(rows[0].rss_kb, "574864");
        assert_eq!(rows[0].pid, "10131");
        assert_eq!(
            rows[0].command,
            "/Applications/Visual Studio Code.app/Contents/MacOS/Electron"
        );
        assert_eq!(rows[1].command, "/sbin/launchd");
    }

    #[test]
    fn one_sample_writes_the_header_and_one_row_per_process() {
        let path = temporary("sample.tsv");
        let clock = Arc::new(FakeClock::new(1_794_132_005_000));
        let runner = scripted_runner(Arc::clone(&clock));
        let mut sampler = start_sampler(SamplerOptions {
            runner,
            clock,
            path: path.clone(),
            interval_ms: 60_000,
            on_first_failure: Arc::new(|_| {}),
        })
        .expect("the sampler starts");
        sampler.stop();

        let text = std::fs::read_to_string(&path).expect("the file reads");
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(SAMPLES_HEADER.join("\t").as_str()));
        let first: Vec<&str> = lines.next().expect("the first row").split('\t').collect();
        assert_eq!(first.len(), 8);
        assert_eq!(first[0], "2026-11-08T10:00:05Z");
        assert_eq!(first[1], "2.50");
        assert_eq!(first[2], "2.00");
        assert_eq!(first[3], "1.50");
        assert_eq!(first[4], "42.0");
        assert_eq!(first[5], "51200");
        assert_eq!(first[6], "101");
        assert_eq!(
            first[7],
            "/Applications/Busy App.app/Contents/MacOS/Busy App"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_written_file_parses_back_into_one_sample() {
        let path = temporary("round-trip.tsv");
        let clock = Arc::new(FakeClock::new(1_794_132_005_000));
        let runner = scripted_runner(Arc::clone(&clock));
        let mut sampler = start_sampler(SamplerOptions {
            runner,
            clock,
            path: path.clone(),
            interval_ms: 60_000,
            on_first_failure: Arc::new(|_| {}),
        })
        .expect("the sampler starts");
        sampler.stop();

        let samples = read_samples_file(&path).expect("the file parses");
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].seconds, 1_794_132_005.0);
        assert_eq!(samples[0].load1, 2.5);
        assert_eq!(samples[0].processes.len(), 2);
        assert_eq!(samples[0].processes[0].cpu_percent, 42.0);
        assert_eq!(
            samples[0].processes[0].command,
            "/Applications/Busy App.app/Contents/MacOS/Busy App"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rows_with_the_same_time_are_one_sample() {
        let text = format!(
            "{}\n2026-11-08T10:00:05Z\t1.5\t1.0\t0.5\t10.0\t100\t1\tone\n2026-11-08T10:00:05Z\t1.5\t1.0\t0.5\t20.0\t200\t2\ttwo\n2026-11-08T10:00:10Z\t2.5\t2.0\t1.5\t30.0\t300\t3\tthree\n",
            SAMPLES_HEADER.join("\t")
        );
        let samples = parse_samples_file(&text).expect("the file parses");
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].processes.len(), 2);
        assert_eq!(samples[0].load1, 1.5);
        assert_eq!(samples[1].seconds, 1_794_132_010.0);
        assert_eq!(samples[1].processes[0].command, "three");
    }

    #[test]
    fn a_bad_header_or_a_bad_row_is_handled() {
        assert!(parse_samples_file("").is_err());
        assert!(parse_samples_file("time\tload1\n").is_err());

        let text = format!(
            "{}\nnot a time\t1.0\t1.0\t1.0\t1.0\t1\t1\tx\n2026-11-08T10:00:05Z\tbad\t1.0\t1.0\t1.0\t1\t1\tx\n2026-11-08T10:00:05Z\t1.0\t1.0\t1.0\tbad\t1\t1\tx\n",
            SAMPLES_HEADER.join("\t")
        );
        let samples = parse_samples_file(&text).expect("the bad rows are skipped");
        assert_eq!(samples.len(), 1, "a bad cpu percent leaves the sample");
        assert!(samples[0].processes.is_empty());
    }

    #[test]
    fn a_failing_sample_is_reported_once_and_does_not_stop_the_sampler() {
        let path = temporary("failure.tsv");
        let clock = Arc::new(FakeClock::new(0));
        let runner = Arc::new(FakeRunner::always(1).with_clock(Arc::clone(&clock), 0));
        let failures = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = Arc::clone(&failures);
        let sampler = start_sampler(SamplerOptions {
            runner,
            clock,
            path: path.clone(),
            interval_ms: 10,
            on_first_failure: Arc::new(move |message| {
                record
                    .lock()
                    .expect("the record is not poisoned")
                    .push(message);
            }),
        })
        .expect("the sampler starts");
        drop(sampler);
        let failures = failures.lock().expect("the record is not poisoned");
        assert_eq!(failures.len(), 1);
        let lines = std::fs::read_to_string(&path).expect("the file reads");
        assert_eq!(lines.trim().lines().count(), 1, "only the header");
        let _ = std::fs::remove_file(&path);
    }
}
