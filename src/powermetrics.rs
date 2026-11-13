//! Starting and stopping the powermetrics sampler.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::clock::Clock;
use crate::runner::{CaptureOptions, Runner, System};

/// One sample every 500 ms, the interval the analysis expects.
pub const SAMPLE_INTERVAL_MS: u64 = 500;

/// Extra time in the sample limit, so powermetrics stops by itself if the tool dies.
pub const SAMPLE_MARGIN_SECONDS: u64 = 300;

/// Lead time between starting powermetrics and the first block.
pub const START_LEAD_SECONDS: u64 = 3;

/// How long a TERM is given before KILL.
pub const TERMINATION_GRACE_SECONDS: u64 = 10;

/// Two samples are enough to see the shape powermetrics writes.
pub const PREFLIGHT_SAMPLES: u64 = 2;

const MILLISECONDS_PER_SECOND: u64 = 1_000;

/// Samples needed to cover the planned block time plus the margin.
pub fn planned_sample_limit(planned_seconds: u64) -> u64 {
    let milliseconds = (planned_seconds + SAMPLE_MARGIN_SECONDS) * MILLISECONDS_PER_SECOND;
    milliseconds.div_ceil(SAMPLE_INTERVAL_MS)
}

pub fn powermetrics_arguments(limit: u64, output: &Path) -> Vec<String> {
    [
        "sudo",
        "-n",
        "powermetrics",
        "--samplers",
        "cpu_power",
        "-i",
        &SAMPLE_INTERVAL_MS.to_string(),
        "-n",
        &limit.to_string(),
        "-b",
        "0",
        "-f",
        "plist",
        "-o",
        &output.to_string_lossy(),
    ]
    .map(str::to_owned)
    .to_vec()
}

/// A running sampler, stopped once.
pub struct Powermetrics {
    runner: Arc<dyn Runner>,
    clock: Arc<dyn Clock>,
    pid: Option<u32>,
    output: PathBuf,
    stopped: Mutex<bool>,
}

impl Powermetrics {
    pub fn start(system: &System, output: PathBuf, sample_limit: u64) -> Result<Self, String> {
        let spawned = system
            .runner
            .spawn(&powermetrics_arguments(sample_limit, &output))?;
        Ok(Self {
            runner: Arc::clone(&system.runner),
            clock: Arc::clone(&system.clock),
            pid: Some(spawned.pid),
            output,
            stopped: Mutex::new(false),
        })
    }

    /// Stops the sampler with TERM, then KILL, and gives the user the file.
    ///
    /// Safe to call twice, so a signal handler and the normal path can both do it.
    pub fn stop(&self) {
        let mut stopped = self.stopped.lock().expect("the stop lock is not poisoned");
        if *stopped {
            return;
        }
        *stopped = true;

        if let Some(pid) = self.pid {
            let _ = self.runner.capture(
                &["sudo", "-n", "kill", "-TERM"]
                    .map(str::to_owned)
                    .into_iter()
                    .chain([pid.to_string()])
                    .collect::<Vec<_>>(),
                CaptureOptions::default(),
            );
            // Never wait without a limit: a sampler that ignores TERM must not hold the run.
            for _ in 0..TERMINATION_GRACE_SECONDS {
                if !self.is_running(pid) {
                    break;
                }
                self.clock.sleep_ms(MILLISECONDS_PER_SECOND);
            }
            if self.is_running(pid) {
                let _ = self.runner.capture(
                    &["sudo", "-n", "pkill", "-KILL", "-x", "powermetrics"].map(str::to_owned),
                    CaptureOptions::default(),
                );
                let _ = self.runner.capture(
                    &["sudo", "-n", "kill", "-KILL", &pid.to_string()].map(str::to_owned),
                    CaptureOptions::default(),
                );
            }
        }

        if let Some(user) = user_name() {
            let mut chown = ["sudo", "-n", "chown", &user].map(str::to_owned).to_vec();
            chown.push(self.output.to_string_lossy().into_owned());
            let _ = self.runner.capture(&chown, CaptureOptions::default());
        }
    }

    fn is_running(&self, pid: u32) -> bool {
        self.runner
            .capture(
                &["ps", "-p", &pid.to_string()].map(str::to_owned),
                CaptureOptions::default(),
            )
            .map(|captured| captured.succeeded())
            .unwrap_or(false)
    }
}

fn user_name() -> Option<String> {
    std::env::var("USER").ok().filter(|user| !user.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use crate::runner::{Captured, FakeRunner};
    use std::sync::atomic::{AtomicU32, Ordering};

    const PID: u32 = 4_000;

    fn ok() -> Captured {
        Captured {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn sampler(runner: Arc<dyn Runner>, clock: Arc<dyn Clock>) -> Powermetrics {
        Powermetrics {
            runner,
            clock,
            pid: Some(PID),
            output: PathBuf::from("/tmp/power.plist"),
            stopped: Mutex::new(false),
        }
    }

    fn lines(runner: &FakeRunner) -> Vec<String> {
        runner.argv().iter().map(|argv| argv.join(" ")).collect()
    }

    #[test]
    fn the_sample_limit_covers_the_plan_and_the_margin() {
        // 450 s planned, 300 s margin, one sample every 500 ms.
        assert_eq!(planned_sample_limit(450), 1_500);
        assert_eq!(planned_sample_limit(0), 600);
        assert_eq!(planned_sample_limit(1), 602);
    }

    #[test]
    fn powermetrics_runs_under_sudo_with_the_expected_flags() {
        let arguments = powermetrics_arguments(1_500, Path::new("/tmp/power.plist"));
        assert_eq!(
            arguments,
            [
                "sudo",
                "-n",
                "powermetrics",
                "--samplers",
                "cpu_power",
                "-i",
                "500",
                "-n",
                "1500",
                "-b",
                "0",
                "-f",
                "plist",
                "-o",
                "/tmp/power.plist",
            ]
        );
    }

    #[test]
    fn stopping_waits_for_the_process_to_go_and_gives_the_file_back() {
        let clock = Arc::new(FakeClock::new(0));
        let polls = AtomicU32::new(0);
        let runner = Arc::new(FakeRunner::new(move |argv| {
            if argv.first().map(String::as_str) == Some("ps") {
                // Running on the first poll, gone on the second.
                let poll = polls.fetch_add(1, Ordering::SeqCst);
                return Captured {
                    exit_code: i32::from(poll > 0),
                    stdout: String::new(),
                    stderr: String::new(),
                };
            }
            ok()
        }));
        let sampler = sampler(runner.clone(), clock.clone());

        sampler.stop();
        sampler.stop();

        assert_eq!(clock.slept_ms(), 1_000, "one second between the two polls");
        let commands = lines(&runner);
        let user = user_name().expect("the test runs as a user");
        assert_eq!(
            commands,
            [
                format!("sudo -n kill -TERM {PID}"),
                format!("ps -p {PID}"),
                format!("ps -p {PID}"),
                format!("ps -p {PID}"),
                format!("sudo -n chown {user} /tmp/power.plist"),
            ]
        );
    }

    #[test]
    fn stopping_kills_a_sampler_that_ignores_term() {
        let clock = Arc::new(FakeClock::new(0));
        let runner = Arc::new(FakeRunner::new(|argv| Captured {
            exit_code: i32::from(argv.first().map(String::as_str) != Some("ps")),
            stdout: String::new(),
            stderr: String::new(),
        }));
        let sampler = sampler(runner.clone(), clock.clone());

        sampler.stop();

        assert_eq!(clock.slept_ms(), TERMINATION_GRACE_SECONDS * 1_000);
        let commands = lines(&runner);
        assert!(commands.contains(&"sudo -n pkill -KILL -x powermetrics".to_string()));
        assert!(commands.contains(&format!("sudo -n kill -KILL {PID}")));
        assert_eq!(
            commands
                .iter()
                .filter(|line| **line == format!("ps -p {PID}"))
                .count(),
            11
        );
    }
}
