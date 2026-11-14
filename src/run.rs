//! Running the measured blocks, and one measurement from start to table.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

use crate::analysis::{
    Analysis, MINIMUM_CLEAN_ROUNDS, analyze, format_analysis, workloads_short_of_clean_rounds,
};
use crate::blocks::{BlockKind, BlockRow, format_header, format_row};
use crate::cli::{Plan, Shell, format_seconds};
use crate::export::{self, RunInfo};
use crate::gate::{QUIET_LOAD, QUIET_WAIT_LIMIT_SECONDS, QuietWait, wait_for_quiet};
use crate::powermetrics::{Powermetrics, START_LEAD_SECONDS, planned_sample_limit};
use crate::preflight::{
    PowermetricsCheck, PreflightReport, check_powermetrics, run_preflight, start_sudo_keepalive,
};
use crate::runner::{CaptureOptions, System, temporary_path};
use crate::samples::parse_power_log;
use crate::schedule::round_orders;

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
                        cwd: None,
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
    powermetrics: PowermetricsCheck,
}

/// Measures the plan and writes its output folder.
pub fn run(plan: &Plan, system: &System) -> Result<(), String> {
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
    )?;

    let preflight_power = temporary_path("preflight.plist");
    let check = check_powermetrics(system, &preflight_power, &mut |line| println!("{line}"))?;
    let _ = std::fs::remove_file(&preflight_power);

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
    println!("        powermetrics stops by itself after {sample_limit} samples");

    let started_at = timestamp(system)?;
    let mut keepalive = start_sudo_keepalive(system);
    let awake = system.runner.spawn(&[
        "caffeinate".to_owned(),
        "-is".to_owned(),
        "-w".to_owned(),
        std::process::id().to_string(),
    ])?;
    let power = Arc::new(Powermetrics::start(
        system,
        power_path.clone(),
        sample_limit,
    )?);

    // A stop by signal skips the normal path, so the sampler is stopped there too.
    install_signal_handlers(Arc::clone(&power))?;

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
            let analysis = analyze_so_far(&power_path, &rows)?;
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
    keepalive.stop();

    // The sampler is stopped and the file handed back before the final read, so
    // the trailing samples are on disk.
    let rows = measured?;
    let analysis = analyze_so_far(&power_path, &rows)?;
    let ended_at = timestamp(system)?;
    let os_build = check.build.clone();
    let hardware_model = hardware_model(&power_path);
    let meta = Meta {
        benchdown_version: env!("CARGO_PKG_VERSION").to_owned(),
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

fn analyze_so_far(power_path: &Path, rows: &[BlockRow]) -> Result<Analysis, String> {
    let text = std::fs::read_to_string(power_path)
        .map_err(|error| format!("{}: {error}", power_path.display()))?;
    let samples = parse_power_log(&text);
    analyze(&samples, rows, &[])
}

fn build_workloads(plan: &Plan) -> Result<Vec<Workload>, String> {
    plan.workloads
        .iter()
        .map(|workload| {
            let argv = command_argv(&plan.shell, &workload.command);
            if argv.is_empty() {
                return Err(format!(
                    "{} has no command to run: --shell=none splits it on whitespace",
                    workload.name
                ));
            }
            Ok(Workload {
                label: workload.name.clone(),
                argv,
                prepare: workload
                    .prepare
                    .as_ref()
                    .map(|prepare| command_argv(&plan.shell, prepare)),
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

fn command_argv(shell: &Shell, command: &str) -> Vec<String> {
    match shell {
        Shell::Program(program) => vec![program.clone(), "-c".to_owned(), command.to_owned()],
        Shell::None => command.split_whitespace().map(str::to_owned).collect(),
    }
}

fn install_signal_handlers(power: Arc<Powermetrics>) -> Result<(), String> {
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
            eprintln!("Stopped by signal {signal}. Stopping powermetrics.");
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

fn hardware_model(power_path: &Path) -> String {
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
            before_block: &mut |_, _| Ok(()),
            on_block: &mut |_| Ok(()),
        })
        .expect_err("the command fails");

        assert!(error.contains("exit code 3"), "{error}");
        assert!(error.contains("boom"), "{error}");
    }

    #[test]
    fn a_shell_command_runs_through_the_shell() {
        assert_eq!(
            command_argv(&Shell::Program("sh".to_owned()), "echo hi"),
            ["sh", "-c", "echo hi"]
        );
    }

    #[test]
    fn a_shell_none_command_splits_on_whitespace() {
        assert_eq!(command_argv(&Shell::None, "echo  hi"), ["echo", "hi"]);
        assert!(command_argv(&Shell::None, "   ").is_empty());
    }
}
