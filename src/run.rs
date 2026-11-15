//! Running the measured blocks, and one measurement from start to table.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

use crate::analysis::{
    Analysis, Limits, MINIMUM_CLEAN_ROUNDS, analyze, format_analysis,
    workloads_short_of_clean_rounds,
};
use crate::blocks::{BlockKind, BlockRow, format_header, format_row};
use crate::cli::{Plan, command_argv, format_seconds};
use crate::export::{self, RunInfo};
use crate::gate::{
    QUIET_LOAD, QUIET_WAIT_LIMIT_SECONDS, QuietWait, find_contaminated_blocks, wait_for_quiet,
};
use crate::meter::{Sampler, open as open_meter};
use crate::powermetrics::{
    Powermetrics, SAMPLE_INTERVAL_MS, START_LEAD_SECONDS, planned_sample_limit,
};
use crate::preflight::{
    PowermetricsCheck, PreflightReport, check_powermetrics, run_preflight, start_sudo_keepalive,
};
use crate::runner::{CaptureOptions, System, temporary_path};
use crate::samples::{PowerSample, parse_power_log};
use crate::schedule::round_orders;
use crate::sensor::Sensor;
use crate::session::sampler::{SESSION_SAMPLES_ENV, read_samples_file};

const MILLISECONDS_PER_SECOND: u64 = 1_000;

/// Samples kept after the last block, so its final interval is complete in the log.
const TRAILING_SAMPLE_SECONDS: u64 = 2;

/// Exit codes for a run stopped by a signal, as a shell reports them.
const SIGNAL_EXIT_CODES: [(i32, i32); 3] = [
    (signal_hook::consts::SIGINT, 130),
    (signal_hook::consts::SIGTERM, 143),
    (signal_hook::consts::SIGHUP, 129),
];

/// Extra rounds at most, each for the workloads still short of clean rounds.
const MAXIMUM_EXTRA_ROUNDS: u64 = 3;

/// One command to measure, with what it needs before a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workload {
    pub label: String,
    /// The command line run during the work phase.
    pub argv: Vec<String>,
    /// Run before each of this workload's blocks.
    pub prepare: Option<Vec<String>>,
}

/// How one pass over a set of rounds is run.
pub struct BlockOptions<'a> {
    pub system: &'a System,
    pub workloads: &'a [Workload],
    pub orders: &'a [Vec<String>],
    /// The number the first round gets, so extra rounds continue the planned numbering.
    pub first_round: u64,
    pub settle_seconds: u64,
    pub idle_seconds: u64,
    pub work_seconds: u64,
    /// Directory the measured commands run in.
    pub cwd: Option<&'a Path>,
    /// Variables added to the measured commands' environment.
    pub env: &'a [(String, String)],
    /// Start the measured commands with an empty environment, then add `env`.
    pub env_clear: bool,
    /// Runs before each block's prepare, such as a wait for a quiet machine.
    pub before_block: &'a mut dyn FnMut(u64, &str) -> Result<(), String>,
    pub on_block: &'a mut dyn FnMut(&BlockRow) -> Result<(), String>,
}

/// Runs the rounds in the given order and returns every block row.
pub fn run_blocks(options: BlockOptions<'_>) -> Result<Vec<BlockRow>, String> {
    if options.workloads.is_empty() {
        return Err("An energy run needs at least one workload".to_owned());
    }
    for (index, workload) in options.workloads.iter().enumerate() {
        if options.workloads[..index]
            .iter()
            .any(|other| other.label == workload.label)
        {
            return Err(format!("Two workloads share the label {}", workload.label));
        }
    }

    let clock = Arc::clone(&options.system.clock);
    let runner = Arc::clone(&options.system.runner);
    let mut rows = Vec::new();

    for (index, order) in options.orders.iter().enumerate() {
        let round = options.first_round + index as u64;
        for label in order {
            let workload = options
                .workloads
                .iter()
                .find(|workload| &workload.label == label)
                .ok_or_else(|| format!("No workload for label {label}"))?;

            (options.before_block)(round, label)?;

            if let Some(prepare) = &workload.prepare {
                let result = runner.capture(prepare, CaptureOptions::default())?;
                if !result.succeeded() {
                    return Err(format!(
                        "{label} stopped the run before its block with exit code {}:\n{}",
                        result.exit_code,
                        result.stderr.trim()
                    ));
                }
            }
            clock.sleep_ms(options.settle_seconds * MILLISECONDS_PER_SECOND);

            let idle_start = clock.now_ms();
            clock.sleep_ms(options.idle_seconds * MILLISECONDS_PER_SECOND);
            let idle_end = clock.now_ms();
            let idle_row = BlockRow {
                round,
                label: label.clone(),
                kind: BlockKind::Idle,
                start: to_seconds(idle_start),
                end: to_seconds(idle_end),
                count: 0,
            };
            rows.push(idle_row.clone());
            (options.on_block)(&idle_row)?;

            let work_start = clock.now_ms();
            let deadline = work_start + options.work_seconds * MILLISECONDS_PER_SECOND;
            let mut count = 0;
            while clock.now_ms() < deadline {
                let result = runner.capture(
                    &workload.argv,
                    CaptureOptions {
                        cwd: options.cwd,
                        env: options.env,
                        clear_env: options.env_clear,
                        discard_stdout: true,
                    },
                )?;
                if !result.succeeded() {
                    return Err(format!(
                        "{label} stopped the run with exit code {}:\n{}",
                        result.exit_code,
                        result.stderr.trim()
                    ));
                }
                count += 1;
            }
            let work_end = clock.now_ms();
            if count == 0 {
                return Err(format!("{label} finished no run inside its work phase"));
            }

            let work_row = BlockRow {
                round,
                label: label.clone(),
                kind: BlockKind::Work,
                start: to_seconds(work_start),
                end: to_seconds(work_end),
                count,
            };
            rows.push(work_row.clone());
            (options.on_block)(&work_row)?;
        }
    }

    Ok(rows)
}

fn to_seconds(milliseconds: u64) -> f64 {
    milliseconds as f64 / MILLISECONDS_PER_SECOND as f64
}

/// What one run recorded, written to `meta.json`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    benchdown_version: String,
    sensor: String,
    arguments: Vec<String>,
    os_build: String,
    hardware_model: String,
    preflight: PreflightRecord,
    started_at: String,
    ended_at: String,
    extra_rounds: u64,
    quiet_waits: Vec<QuietWait>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PreflightRecord {
    #[serde(flatten)]
    machine: PreflightReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    powermetrics: Option<PowermetricsCheck>,
}

/// Measures the plan and writes its output folder.
pub fn run(plan: &Plan, system: &System) -> Result<(), String> {
    // The session wrapper writes the samples while this run is in progress, so
    // the file is checked once here and read again when the round is analyzed.
    let session_samples = session_samples_path()?;
    let started_ms = system.clock.now_ms();
    let directory = match &plan.out {
        Some(directory) => directory.clone(),
        None => default_output_directory(system)?,
    };
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    let power_path = directory.join("power.plist");
    let blocks_path = directory.join("blocks.tsv");
    let analysis_path = directory.join("analysis.json");
    let meta_path = directory.join("meta.json");

    println!("Results: {}", directory.display());
    let machine = run_preflight(
        system,
        &directory,
        &mut |line| println!("{line}"),
        std::io::stdin().is_terminal(),
        plan.sensor.needs_sudo(),
    )?;

    let check = if plan.sensor == Sensor::Powermetrics {
        let preflight_power = temporary_path("preflight.plist");
        let check = check_powermetrics(system, &preflight_power, &mut |line| println!("{line}"))?;
        let _ = std::fs::remove_file(&preflight_power);
        Some(check)
    } else {
        None
    };

    std::fs::write(&blocks_path, format!("{}\n", format_header()))
        .map_err(|error| format!("{}: {error}", blocks_path.display()))?;

    let workloads = build_workloads(plan)?;
    let planned_seconds = plan.total_seconds();
    let extra_blocks = workloads.len() as u64 * MAXIMUM_EXTRA_ROUNDS;
    let most_blocks = planned_seconds / plan.seconds_per_block() + extra_blocks;
    let longest_seconds = planned_seconds
        + extra_blocks * plan.seconds_per_block()
        + most_blocks * QUIET_WAIT_LIMIT_SECONDS;
    let sample_limit = planned_sample_limit(longest_seconds);
    println!(
        "Plan:   {} of blocks, at most {} with extra rounds and quiet waits",
        format_seconds(planned_seconds),
        format_seconds(longest_seconds)
    );
    match plan.sensor {
        Sensor::Powermetrics => {
            println!("        powermetrics stops by itself after {sample_limit} samples")
        }
        sensor => {
            println!("        {sensor} is sampled every {SAMPLE_INTERVAL_MS} ms and needs no sudo")
        }
    }

    let started_at = timestamp(system)?;
    let mut keepalive = plan
        .sensor
        .needs_sudo()
        .then(|| start_sudo_keepalive(system));
    let awake = system.runner.spawn(&[
        "caffeinate".to_owned(),
        "-is".to_owned(),
        "-w".to_owned(),
        std::process::id().to_string(),
    ])?;
    let power = Power::start(plan.sensor, system, &power_path, sample_limit)?;

    // A stop by signal skips the normal path, so the sampler is stopped there too.
    install_signal_handlers(power.clone(), plan.sensor)?;

    let mut quiet_waits = Vec::new();
    let mut extra_rounds = 0;

    let measured = (|quiet_waits: &mut Vec<QuietWait>| -> Result<Vec<BlockRow>, String> {
        system
            .clock
            .sleep_ms(START_LEAD_SECONDS * MILLISECONDS_PER_SECOND);

        let orders = round_orders(
            &workload_labels(&workloads),
            plan.rounds as usize,
            plan.seed,
        );
        let mut rows = run_rounds(
            plan,
            system,
            &workloads,
            &orders,
            1,
            &blocks_path,
            quiet_waits,
        )?;
        system
            .clock
            .sleep_ms(TRAILING_SAMPLE_SECONDS * MILLISECONDS_PER_SECOND);

        // A run planned below the minimum, such as --check, is not topped up.
        let tops_up = plan.rounds >= MINIMUM_CLEAN_ROUNDS as u64;
        while tops_up && extra_rounds < MAXIMUM_EXTRA_ROUNDS {
            let analysis = analyze_so_far(
                &power.samples()?,
                &rows,
                session_samples.as_deref(),
                Limits::for_sensor(plan.sensor),
            )?;
            let short = workloads_short_of_clean_rounds(&analysis);
            if short.is_empty() {
                break;
            }
            extra_rounds += 1;
            let round = plan.rounds + extra_rounds;
            println!(
                "Extra round {round}: fewer than {MINIMUM_CLEAN_ROUNDS} clean rounds for {}",
                short.join(", ")
            );
            let orders = round_orders(&short, 1, plan.seed.wrapping_add(round as u32));
            rows.extend(run_rounds(
                plan,
                system,
                &workloads,
                &orders,
                round,
                &blocks_path,
                quiet_waits,
            )?);
            system
                .clock
                .sleep_ms(TRAILING_SAMPLE_SECONDS * MILLISECONDS_PER_SECOND);
        }

        Ok(rows)
    })(&mut quiet_waits);

    power.stop();
    stop_child(system, awake.pid);
    if let Some(keepalive) = keepalive.as_mut() {
        keepalive.stop();
    }

    // The sampler is stopped and the file handed back before the final read, so
    // the trailing samples are on disk.
    let rows = measured?;
    // A counter sensor keeps its samples in memory; write them beside the run's
    // other files so the run can be analyzed again.
    if plan.sensor != Sensor::Powermetrics {
        let samples_file = directory.join("samples.tsv");
        std::fs::write(
            &samples_file,
            crate::samples::format_samples(&power.samples()?),
        )
        .map_err(|error| format!("{}: {error}", samples_file.display()))?;
        println!("Samples: {}", samples_file.display());
    }
    let analysis = analyze_so_far(
        &power.samples()?,
        &rows,
        session_samples.as_deref(),
        Limits::for_sensor(plan.sensor),
    )?;
    let ended_at = timestamp(system)?;
    let os_build = match &check {
        Some(check) => check.build.clone(),
        None => os_build(system),
    };
    let hardware_model = hardware_model(system, &power_path, plan.sensor);
    let meta = Meta {
        benchdown_version: env!("CARGO_PKG_VERSION").to_owned(),
        sensor: plan.sensor.to_string(),
        arguments: std::env::args().skip(1).collect(),
        os_build: os_build.clone(),
        hardware_model: hardware_model.clone(),
        preflight: PreflightRecord {
            machine,
            powermetrics: check,
        },
        started_at: started_at.clone(),
        ended_at,
        extra_rounds,
        quiet_waits,
    };
    write_json(&meta_path, &meta)?;
    write_json(&analysis_path, &analysis)?;

    if let Some(path) = &plan.export_json {
        let workloads: Vec<(String, String)> = plan
            .workloads
            .iter()
            .map(|workload| (workload.name.clone(), workload.command.clone()))
            .collect();
        let (os, architecture) = platform(system);
        let export = export::build(
            &analysis,
            RunInfo {
                workloads: &workloads,
                started_at: &started_at,
                os_build: &os_build,
                hardware_model: &hardware_model,
                os: &os,
                architecture: &architecture,
            },
        )?;
        export::write(path, &export)?;
        println!("Exported: {}", path.display());
    }

    println!();
    println!("Sensor: {} — {}", plan.sensor, plan.sensor.describes());
    println!();
    println!("{}", format_analysis(&analysis).join("\n"));
    println!();
    let elapsed_seconds =
        system.clock.now_ms().saturating_sub(started_ms) / MILLISECONDS_PER_SECOND;
    println!("Total: {}", format_seconds(elapsed_seconds));
    Ok(())
}

fn run_rounds(
    plan: &Plan,
    system: &System,
    workloads: &[Workload],
    orders: &[Vec<String>],
    first_round: u64,
    blocks_path: &Path,
    quiet_waits: &mut Vec<QuietWait>,
) -> Result<Vec<BlockRow>, String> {
    let total: usize = orders.iter().map(Vec::len).sum();
    let mut done = 0;

    run_blocks(BlockOptions {
        system,
        workloads,
        orders,
        first_round,
        settle_seconds: plan.settle_seconds,
        idle_seconds: plan.idle_seconds,
        work_seconds: plan.work_seconds,
        cwd: plan.cwd.as_deref(),
        env: &plan.env,
        env_clear: plan.env_clear,
        before_block: &mut |round, label| {
            let wait = wait_for_quiet(round, label, &mut load_reader(system), &|milliseconds| {
                system.clock.sleep_ms(milliseconds)
            })?;
            if let Some(wait) = wait {
                let outcome = if wait.timed_out {
                    "still busy, running anyway".to_owned()
                } else {
                    format!("now {}", wait.load_after)
                };
                println!(
                    "  waited {}s for load {} to fall below {QUIET_LOAD}: {outcome}",
                    wait.waited_seconds, wait.load_before
                );
                quiet_waits.push(wait);
            }
            Ok(())
        },
        on_block: &mut |row| {
            append_block(blocks_path, row)?;
            if row.kind == BlockKind::Work {
                done += 1;
                println!("  block {done}/{total}: {}, {} runs", row.label, row.count);
            }
            Ok(())
        },
    })
}

/// Reads the one-minute load average, or fails with the command's error.
fn load_reader(system: &System) -> impl FnMut() -> Result<f64, String> {
    let runner = Arc::clone(&system.runner);
    move || {
        let output = runner.capture(
            &["sysctl", "-n", "vm.loadavg"].map(str::to_owned),
            CaptureOptions::default(),
        )?;
        if !output.succeeded() {
            return Err(format!(
                "sysctl failed with exit code {}: {}",
                output.exit_code,
                output.stderr.trim()
            ));
        }
        output
            .stdout
            .split(|character: char| !(character.is_ascii_digit() || character == '.'))
            .find_map(|part| part.parse().ok())
            .ok_or_else(|| {
                format!(
                    "Could not parse the load average from: {}",
                    output.stdout.trim()
                )
            })
    }
}

fn append_block(path: &Path, row: &BlockRow) -> Result<(), String> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    writeln!(file, "{}", format_row(row)).map_err(|error| format!("{}: {error}", path.display()))
}

/// The running power source.
///
/// powermetrics writes a plist that is parsed again for each extra round; the
/// counter sensors sample into memory. Both hand back the same samples.
#[derive(Clone)]
enum Power {
    Powermetrics {
        sampler: Arc<Powermetrics>,
        path: PathBuf,
    },
    Sampler(Arc<Sampler>),
}

impl Power {
    fn start(
        sensor: Sensor,
        system: &System,
        power_path: &Path,
        sample_limit: u64,
    ) -> Result<Self, String> {
        if sensor == Sensor::Powermetrics {
            return Ok(Self::Powermetrics {
                sampler: Arc::new(Powermetrics::start(
                    system,
                    power_path.to_path_buf(),
                    sample_limit,
                )?),
                path: power_path.to_path_buf(),
            });
        }
        Ok(Self::Sampler(Arc::new(Sampler::start(
            open_meter(sensor)?,
            SAMPLE_INTERVAL_MS,
        )?)))
    }

    /// The samples taken so far, in time order.
    fn samples(&self) -> Result<Vec<PowerSample>, String> {
        match self {
            Self::Powermetrics { path, .. } => {
                let text = std::fs::read_to_string(path)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                Ok(parse_power_log(&text))
            }
            Self::Sampler(sampler) => {
                let samples = sampler.samples();
                match (samples.is_empty(), sampler.failure()) {
                    (true, Some(failure)) => Err(format!("the sensor stopped: {failure}")),
                    _ => Ok(samples),
                }
            }
        }
    }

    /// Safe to call twice.
    fn stop(&self) {
        match self {
            Self::Powermetrics { sampler, .. } => sampler.stop(),
            Self::Sampler(sampler) => sampler.stop(),
        }
    }
}

fn analyze_so_far(
    samples: &[PowerSample],
    rows: &[BlockRow],
    session_samples: Option<&Path>,
    limits: Limits,
) -> Result<Analysis, String> {
    let contaminated = match session_samples {
        Some(path) => find_contaminated_blocks(&read_samples_file(path)?, rows),
        None => Vec::new(),
    };
    analyze(samples, rows, &contaminated, limits)
}

/// The samples file named by the session, checked before the sampler starts.
fn session_samples_path() -> Result<Option<PathBuf>, String> {
    session_samples_from(std::env::var_os(SESSION_SAMPLES_ENV))
}

/// Splits the environment lookup out, so a test can drive it without changing the process.
fn session_samples_from(value: Option<std::ffi::OsString>) -> Result<Option<PathBuf>, String> {
    let Some(path) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    read_samples_file(&path).map_err(|error| {
        format!(
            "{SESSION_SAMPLES_ENV} is set to {} but its samples cannot be read: {error}",
            path.display()
        )
    })?;
    Ok(Some(path))
}

fn build_workloads(plan: &Plan) -> Result<Vec<Workload>, String> {
    plan.workloads
        .iter()
        .map(|workload| {
            Ok(Workload {
                label: workload.name.clone(),
                argv: command_argv(&plan.shell, &workload.command)
                    .map_err(|error| format!("{}: {error}", workload.name))?,
                prepare: workload
                    .prepare
                    .as_ref()
                    .map(|prepare| command_argv(&plan.shell, prepare))
                    .transpose()
                    .map_err(|error| format!("{}: prepare {error}", workload.name))?,
            })
        })
        .collect()
}

fn workload_labels(workloads: &[Workload]) -> Vec<String> {
    workloads
        .iter()
        .map(|workload| workload.label.clone())
        .collect()
}

fn install_signal_handlers(power: Power, sensor: Sensor) -> Result<(), String> {
    let mut signals =
        signal_hook::iterator::Signals::new(SIGNAL_EXIT_CODES.map(|(signal, _)| signal))
            .map_err(|error| format!("could not watch for signals: {error}"))?;
    std::thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            let code = SIGNAL_EXIT_CODES
                .iter()
                .find(|(number, _)| *number == signal)
                .map(|(_, code)| *code)
                .unwrap_or(1);
            eprintln!("Stopped by signal {signal}. Stopping the {sensor} sampler.");
            power.stop();
            std::process::exit(code);
        }
    });
    Ok(())
}

fn stop_child(system: &System, pid: u32) {
    let _ = system.runner.capture(
        &["kill", "-TERM", &pid.to_string()].map(str::to_owned),
        CaptureOptions::default(),
    );
}

fn timestamp(system: &System) -> Result<String, String> {
    let captured = system.runner.capture(
        &["date", "-u", "+%Y-%m-%dT%H:%M:%SZ"].map(str::to_owned),
        CaptureOptions::default(),
    )?;
    if !captured.succeeded() {
        return Err("date failed".to_owned());
    }
    Ok(captured.stdout.trim().to_owned())
}

fn default_output_directory(system: &System) -> Result<PathBuf, String> {
    let captured = system.runner.capture(
        &["date", "+%Y%m%d-%H%M%S"].map(str::to_owned),
        CaptureOptions::default(),
    )?;
    if !captured.succeeded() {
        return Err("date failed".to_owned());
    }
    Ok(PathBuf::from(format!(
        "benchdown-{}",
        captured.stdout.trim()
    )))
}

/// The macOS build, from `sysctl`, when no powermetrics sample supplies it.
fn os_build(system: &System) -> String {
    system
        .runner
        .capture(
            &["sysctl", "-n", "kern.osversion"].map(str::to_owned),
            CaptureOptions::default(),
        )
        .map(|output| {
            if output.succeeded() {
                output.stdout.trim().to_owned()
            } else {
                "unknown".to_owned()
            }
        })
        .unwrap_or_else(|_| "unknown".to_owned())
}

fn hardware_model(system: &System, power_path: &Path, sensor: Sensor) -> String {
    if sensor != Sensor::Powermetrics {
        return system
            .runner
            .capture(
                &["sysctl", "-n", "hw.model"].map(str::to_owned),
                CaptureOptions::default(),
            )
            .map(|output| {
                if output.succeeded() {
                    output.stdout.trim().to_owned()
                } else {
                    "unknown".to_owned()
                }
            })
            .unwrap_or_else(|_| "unknown".to_owned());
    }
    let Ok(text) = std::fs::read_to_string(power_path) else {
        return "unknown".to_owned();
    };
    crate::samples::parse_records(&text)
        .first()
        .and_then(|record| record.as_dictionary())
        .and_then(|record| record.get("hw_model"))
        .and_then(|value| value.as_string())
        .map(str::to_owned)
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The operating system and architecture hyperfine records in its metadata.
fn platform(system: &System) -> (String, String) {
    (
        uname(system, "-s", std::env::consts::OS),
        uname(system, "-m", std::env::consts::ARCH),
    )
}

fn uname(system: &System, flag: &str, fallback: &str) -> String {
    system
        .runner
        .capture(
            &["uname".to_owned(), flag.to_owned()],
            CaptureOptions::default(),
        )
        .ok()
        .filter(|captured| captured.succeeded())
        .map(|captured| captured.stdout.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    std::fs::write(path, format!("{text}\n"))
        .map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use crate::runner::FakeRunner;

    fn workload() -> Workload {
        Workload {
            label: "x".to_owned(),
            argv: vec!["true".to_owned()],
            prepare: None,
        }
    }

    fn orders() -> Vec<Vec<String>> {
        vec![vec!["x".to_owned()]]
    }

    #[test]
    fn a_round_records_an_idle_block_then_a_work_block() {
        let clock = Arc::new(FakeClock::new(1_000));
        let runner = Arc::new(FakeRunner::always(0).with_clock(Arc::clone(&clock), 500));
        let system = System {
            runner: runner.clone(),
            clock: clock.clone(),
        };
        let workloads = [workload()];
        let orders = orders();
        let mut observed = Vec::new();

        let rows = run_blocks(BlockOptions {
            system: &system,
            workloads: &workloads,
            orders: &orders,
            first_round: 1,
            settle_seconds: 1,
            idle_seconds: 2,
            work_seconds: 3,
            cwd: None,
            env: &[],
            env_clear: false,
            before_block: &mut |_, _| Ok(()),
            on_block: &mut |row| {
                observed.push(row.clone());
                Ok(())
            },
        })
        .expect("the round runs");

        assert_eq!(rows, observed, "each row reaches the writer as it finishes");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, BlockKind::Idle);
        assert_eq!((rows[0].start, rows[0].end), (2.0, 4.0));
        assert_eq!(rows[0].count, 0);
        assert_eq!(rows[1].kind, BlockKind::Work);
        assert_eq!((rows[1].start, rows[1].end), (4.0, 7.0));
        assert_eq!(rows[1].count, 6, "one run every 500 ms for three seconds");
        assert_eq!(rows[1].round, 1);
        assert_eq!(clock.slept_ms(), 3_000, "settle plus the idle block only");
    }

    #[test]
    fn a_prepare_that_fails_stops_the_run() {
        let clock = Arc::new(FakeClock::new(0));
        let runner = Arc::new(FakeRunner::new(|argv| crate::runner::Captured {
            exit_code: if argv.first().map(String::as_str) == Some("false") {
                2
            } else {
                0
            },
            stdout: String::new(),
            stderr: "prepare went wrong".to_owned(),
        }));
        let system = System {
            runner: runner.clone(),
            clock,
        };
        let workloads = [Workload {
            label: "x".to_owned(),
            argv: vec!["true".to_owned()],
            prepare: Some(vec!["false".to_owned()]),
        }];
        let orders = orders();

        let error = run_blocks(BlockOptions {
            system: &system,
            workloads: &workloads,
            orders: &orders,
            first_round: 1,
            settle_seconds: 1,
            idle_seconds: 1,
            work_seconds: 1,
            cwd: None,
            env: &[],
            env_clear: false,
            before_block: &mut |_, _| Ok(()),
            on_block: &mut |_| Ok(()),
        })
        .expect_err("the prepare fails");

        assert!(error.contains("before its block"), "{error}");
        assert!(error.contains("prepare went wrong"), "{error}");
    }

    #[test]
    fn a_command_that_fails_stops_the_run_with_its_stderr() {
        let clock = Arc::new(FakeClock::new(0));
        let runner = Arc::new(FakeRunner::new(|_| crate::runner::Captured {
            exit_code: 3,
            stdout: String::new(),
            stderr: "boom".to_owned(),
        }));
        let system = System { runner, clock };
        let workloads = [workload()];
        let orders = orders();

        let error = run_blocks(BlockOptions {
            system: &system,
            workloads: &workloads,
            orders: &orders,
            first_round: 1,
            settle_seconds: 0,
            idle_seconds: 0,
            work_seconds: 1,
            cwd: None,
            env: &[],
            env_clear: false,
            before_block: &mut |_, _| Ok(()),
            on_block: &mut |_| Ok(()),
        })
        .expect_err("the command fails");

        assert!(error.contains("exit code 3"), "{error}");
        assert!(error.contains("boom"), "{error}");
    }

    #[test]
    fn a_measured_command_gets_the_cwd_and_environment_but_prepare_does_not() {
        let clock = Arc::new(FakeClock::new(0));
        let runner = Arc::new(FakeRunner::always(0).with_clock(Arc::clone(&clock), 500));
        let system = System {
            runner: runner.clone(),
            clock,
        };
        let workloads = [Workload {
            label: "x".to_owned(),
            argv: vec!["true".to_owned()],
            prepare: Some(vec!["true".to_owned()]),
        }];
        let orders = orders();
        let cwd = Path::new("/measured");
        let env = [
            ("PATH".to_owned(), "/bin".to_owned()),
            ("HOME".to_owned(), "/home/user".to_owned()),
        ];

        run_blocks(BlockOptions {
            system: &system,
            workloads: &workloads,
            orders: &orders,
            first_round: 1,
            settle_seconds: 0,
            idle_seconds: 0,
            work_seconds: 1,
            cwd: Some(cwd),
            env: &env,
            env_clear: false,
            before_block: &mut |_, _| Ok(()),
            on_block: &mut |_| Ok(()),
        })
        .expect("the round runs");

        let captures = runner.captures();
        assert_eq!(captures.len(), 3, "one prepare and two measured runs");
        let prepare = &captures[0];
        assert_eq!(prepare.argv, ["true"]);
        assert_eq!(prepare.cwd, None, "prepare keeps benchdown's directory");
        assert!(
            prepare.env.is_empty(),
            "prepare keeps benchdown's environment"
        );
        assert!(!prepare.clear_env);
        assert!(!prepare.discard_stdout);

        for measured in &captures[1..] {
            assert_eq!(measured.cwd.as_deref(), Some(cwd));
            assert_eq!(measured.env, env);
            assert!(!measured.clear_env);
            assert!(measured.discard_stdout);
        }
    }

    #[test]
    fn clearing_the_environment_records_exactly_the_listed_variables() {
        let clock = Arc::new(FakeClock::new(0));
        let runner = Arc::new(FakeRunner::always(0).with_clock(Arc::clone(&clock), 500));
        let system = System {
            runner: runner.clone(),
            clock,
        };
        let workloads = [workload()];
        let orders = orders();
        let env = [("ONLY".to_owned(), "1".to_owned())];

        run_blocks(BlockOptions {
            system: &system,
            workloads: &workloads,
            orders: &orders,
            first_round: 1,
            settle_seconds: 0,
            idle_seconds: 0,
            work_seconds: 1,
            cwd: None,
            env: &env,
            env_clear: true,
            before_block: &mut |_, _| Ok(()),
            on_block: &mut |_| Ok(()),
        })
        .expect("the round runs");

        let captures = runner.captures();
        assert!(!captures.is_empty());
        for measured in &captures {
            assert!(measured.clear_env, "the environment starts empty");
            assert_eq!(measured.env, env);
        }
    }

    #[test]
    fn the_session_samples_path_comes_from_the_environment() {
        assert_eq!(session_samples_from(None), Ok(None));
        assert_eq!(
            session_samples_from(Some(std::ffi::OsString::new())),
            Ok(None)
        );
        let path = std::env::temp_dir().join(format!(
            "benchdown-run-test-{}-env-samples.tsv",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "time\tload1\tload5\tload15\tcpu_percent\trss_kb\tpid\tcommand\n",
        )
        .expect("the samples file writes");
        assert_eq!(
            session_samples_from(Some(path.clone().into_os_string())),
            Ok(Some(path.clone()))
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unreadable_session_samples_path_names_the_variable_and_the_file() {
        let missing = std::env::temp_dir().join(format!(
            "benchdown-run-test-{}-missing-samples.tsv",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&missing);
        let error = session_samples_from(Some(missing.clone().into_os_string()))
            .expect_err("a missing samples file is refused");
        assert!(error.contains(SESSION_SAMPLES_ENV), "{error}");
        assert!(error.contains("missing-samples.tsv"), "{error}");
    }

    #[test]
    fn a_session_samples_file_marks_its_block_contaminated() {
        let blocks = crate::blocks::parse_blocks(include_str!("../tests/fixtures/blocks.tsv"))
            .expect("the fixture block log parses");
        let power_path = std::env::temp_dir().join(format!(
            "benchdown-run-test-{}-power.plist",
            std::process::id()
        ));
        std::fs::write(&power_path, include_str!("../tests/fixtures/power.plist"))
            .expect("the power fixture writes");
        let samples_path = std::env::temp_dir().join(format!(
            "benchdown-run-test-{}-samples.tsv",
            std::process::id()
        ));
        std::fs::write(
            &samples_path,
            "time\tload1\tload5\tload15\tcpu_percent\trss_kb\tpid\tcommand\n\
             2026-11-08T10:00:05Z\t4.4\t2.0\t1.5\t1.0\t512\t101\t/usr/sbin/mds\n",
        )
        .expect("the samples file writes");

        let text = std::fs::read_to_string(&power_path).expect("the power fixture reads");
        let samples = parse_power_log(&text);
        let analysis = analyze_so_far(&samples, &blocks, Some(&samples_path), Limits::default())
            .expect("the fixture analyzes");

        assert_eq!(analysis.contaminated_blocks.len(), 1);
        assert_eq!(analysis.dropped_rounds, [1]);
        assert_eq!(analysis.workloads[0].clean_rounds, 0);
        let _ = std::fs::remove_file(&power_path);
        let _ = std::fs::remove_file(&samples_path);
    }

    #[test]
    fn the_meta_preflight_holds_battery_and_thermal_state_beside_the_power_source() {
        let record = PreflightRecord {
            machine: PreflightReport {
                power_source: "Now drawing from 'AC Power'".to_owned(),
                battery: crate::preflight::BatteryState {
                    present: true,
                    percent: Some(82),
                    state: "AC attached; not charging".to_owned(),
                },
                thermal: crate::preflight::ThermalState {
                    warning: None,
                    performance_warning: None,
                    cpu_speed_limit: Some(100),
                    cpu_power_limit: Some(100),
                },
                load_average: 1.69,
                free_bytes: 5_212_016_640,
            },
            powermetrics: Some(PowermetricsCheck {
                sample_count: 2,
                build: "24G517".to_owned(),
                build_verified: true,
                issues: Vec::new(),
            }),
        };

        let json = serde_json::to_value(&record).expect("the record serializes");
        assert_eq!(json["powerSource"], "Now drawing from 'AC Power'");
        assert_eq!(json["battery"]["percent"], 82);
        assert_eq!(json["battery"]["state"], "AC attached; not charging");
        assert_eq!(json["thermal"]["cpuSpeedLimit"], 100);
        assert_eq!(json["thermal"]["cpuPowerLimit"], 100);
        assert_eq!(json["powermetrics"]["sampleCount"], 2);
    }
}
