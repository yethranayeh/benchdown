//! Checks the machine and the sampler before a real energy run.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use serde::Serialize;

use crate::powermetrics::{PREFLIGHT_SAMPLES, powermetrics_arguments};
use crate::runner::{CaptureOptions, System};
use crate::samples::{number, parse_power_log, parse_records};

/// Below this many free bytes the run stops.
pub const MINIMUM_FREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// A one-minute load above this warns that other work will widen the spread.
pub const MAXIMUM_LOAD_AVERAGE: f64 = 3.0;

/// The sudo ticket is refreshed at this interval while the run is active.
pub const SUDO_REFRESH_MS: u64 = 50_000;

/// Number of busiest processes printed before a run.
pub const BUSIEST_PROCESS_COUNT: usize = 5;

/// Width of one busiest-process line.
pub const PROCESS_LINE_WIDTH: usize = 70;

/// Highest plausible combined power in milliwatts.
pub const MAXIMUM_POWER_MW: f64 = 200_000.0;

/// Lowest plausible combined power in milliwatts.
pub const MINIMUM_POWER_MW: f64 = 0.0;

/// Allowed relative difference between elapsed_ns and the sampling interval.
pub const ELAPSED_TOLERANCE: f64 = 0.2;

const BYTES_PER_KILOBYTE: u64 = 1_024;
const KILOBYTES_PER_MEGABYTE: u64 = 1_024;
const NANOSECONDS_PER_SECOND: f64 = 1e9;
const MILLISECONDS_PER_SECOND: f64 = 1_000.0;
const LOAD_WARNING_DECIMALS: usize = 2;
const MILLISECONDS_PER_SECOND_WHOLE: u64 = 1_000;

/// The key paths a usable powermetrics sample must carry.
pub const REQUIRED_SAMPLE_PATHS: [&str; 5] = [
    "elapsed_ns",
    "timestamp",
    "kern_osversion",
    "hw_model",
    "processor.combined_power",
];

/// A sample that powermetrics recorded on an untested macOS build.
const KNOWN_GOOD: &str = include_str!("../known-good/24G517.plist");

/// What the machine looked like before the run.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightReport {
    pub power_source: String,
    pub load_average: f64,
    pub free_bytes: u64,
}

/// One thing wrong with one sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SampleIssue {
    pub path: String,
    pub problem: String,
}

/// What the two-sample sampler check found.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PowermetricsCheck {
    pub sample_count: usize,
    pub build: String,
    pub build_verified: bool,
    pub issues: Vec<SampleIssue>,
}

/// Prints the machine state, stops on low disk and makes sure sudo has a ticket.
pub fn run_preflight(
    system: &System,
    free_disk_path: &Path,
    write: &mut dyn FnMut(&str),
    stdin_is_terminal: bool,
) -> Result<PreflightReport, String> {
    let battery = capture(system, &["pmset", "-g", "batt"])?;
    let power_source = battery.lines().next().unwrap_or("").trim().to_owned();
    let load_values = read_load_values(system)?;
    let load_average = load_values.first().copied().unwrap_or(0.0);
    let free_bytes = read_free_bytes(system, free_disk_path)?;
    let busiest = read_busiest_processes(system)?;

    write(&format!("Power:  {power_source}"));
    write(&format!(
        "Load:   {}",
        load_values
            .iter()
            .map(|value| format!("{value:.LOAD_WARNING_DECIMALS$}"))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    write(&format!(
        "Disk:   {} MB free",
        free_bytes / BYTES_PER_KILOBYTE / KILOBYTES_PER_MEGABYTE
    ));
    write("Top CPU right now:");
    for line in &busiest {
        write(&format!("  {line}"));
    }

    if free_bytes < MINIMUM_FREE_BYTES {
        return Err("STOP: less than 2 GB free. Free some disk first.".to_owned());
    }
    if !power_source.contains("AC Power") {
        write("WARNING: not on AC power.");
    }
    if load_average > MAXIMUM_LOAD_AVERAGE {
        write(&format!(
            "WARNING: load is {load_average:.LOAD_WARNING_DECIMALS$}. Agents or builds still running will widen the spread."
        ));
    }

    ensure_sudo_ticket(system, stdin_is_terminal)?;
    Ok(PreflightReport {
        power_source,
        load_average,
        free_bytes,
    })
}

/// Refreshes the sudo ticket in the background until told to stop.
pub struct SudoKeepalive {
    alive: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SudoKeepalive {
    pub fn stop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
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

/// Refreshes the sudo ticket in the background. Stops when dropped.
pub fn start_sudo_keepalive(system: &System) -> SudoKeepalive {
    let alive = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&alive);
    let runner = Arc::clone(&system.runner);
    let thread = std::thread::spawn(move || {
        let mut elapsed = 0;
        while flag.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(
                MILLISECONDS_PER_SECOND_WHOLE,
            ));
            elapsed += MILLISECONDS_PER_SECOND_WHOLE;
            if elapsed >= SUDO_REFRESH_MS {
                elapsed = 0;
                let _ = runner.capture(
                    &["sudo", "-n", "true"].map(str::to_owned),
                    CaptureOptions::default(),
                );
            }
        }
    });

    SudoKeepalive {
        alive,
        thread: Some(thread),
    }
}

pub fn ensure_sudo_ticket(system: &System, stdin_is_terminal: bool) -> Result<(), String> {
    let arguments = ["sudo", "-n", "true"].map(str::to_owned);
    if system
        .runner
        .capture(&arguments, CaptureOptions::default())?
        .succeeded()
    {
        return Ok(());
    }
    if !stdin_is_terminal {
        return Err(
            "sudo needs a password. Run `sudo -v` first, then start the run again.".to_owned(),
        );
    }

    let interactive = ["sudo", "-v"].map(str::to_owned);
    if system.runner.interactive(&interactive)? != 0 {
        return Err("sudo -v failed. Run `sudo -v` first, then start the run again.".to_owned());
    }
    Ok(())
}

/// Takes two samples and checks their shape against the newest known-good sample.
pub fn check_powermetrics(
    system: &System,
    output_path: &Path,
    write: &mut dyn FnMut(&str),
) -> Result<PowermetricsCheck, String> {
    let run = system.runner.capture(
        &powermetrics_arguments(PREFLIGHT_SAMPLES, output_path),
        CaptureOptions::default(),
    )?;
    if !run.succeeded() {
        return Err(format!(
            "powermetrics preflight failed with exit code {}:\n{}",
            run.exit_code,
            run.stderr.trim()
        ));
    }
    if !output_path.exists() {
        return Err(format!("powermetrics wrote no {}", output_path.display()));
    }
    let text = std::fs::read_to_string(output_path)
        .map_err(|error| format!("{}: {error}", output_path.display()))?;
    let samples = parse_power_log(&text);
    if samples.is_empty() {
        return Err("powermetrics wrote no usable samples".to_owned());
    }

    let records = parse_records(&text);
    let issues: Vec<SampleIssue> = records.iter().flat_map(inspect_sample).collect();
    if !issues.is_empty() {
        let known_good = known_good();
        write(&format!(
            "powermetrics sample mismatch ({} samples)",
            samples.len()
        ));
        for line in describe_differences(&records[0], known_good.as_ref()) {
            write(&format!("  {line}"));
        }
        return Err(
            "powermetrics returned an unexpected shape. The key paths above differ.".to_owned(),
        );
    }

    let build = records[0]
        .as_dictionary()
        .and_then(|record| record.get("kern_osversion"))
        .and_then(|value| value.as_string())
        .unwrap_or("unknown")
        .to_owned();
    let build_verified = known_good()
        .and_then(|value| {
            value
                .as_dictionary()
                .and_then(|record| record.get("kern_osversion"))
                .and_then(|value| value.as_string())
                .map(|known| known == build)
        })
        .unwrap_or(false);

    if build_verified {
        write(&format!(
            "powermetrics build {build} matches the known-good sample"
        ));
    } else {
        write(&format!(
            "powermetrics build {build} is untested. After a verified run, add a known-good sample at known-good/{build}.plist"
        ));
    }

    Ok(PowermetricsCheck {
        sample_count: samples.len(),
        build,
        build_verified,
        issues,
    })
}

/// The known-good sample this crate ships, when it parses.
pub fn known_good() -> Option<plist::Value> {
    plist::Value::from_reader_xml(KNOWN_GOOD.as_bytes()).ok()
}

/// Returns the key paths that are missing, the wrong type, or implausible in one sample.
pub fn inspect_sample(value: &plist::Value) -> Vec<SampleIssue> {
    let mut issues = Vec::new();
    let record = value.as_dictionary();

    let expected_seconds = crate::powermetrics::SAMPLE_INTERVAL_MS as f64 / MILLISECONDS_PER_SECOND;
    let elapsed = record.and_then(|record| record.get("elapsed_ns"));
    match elapsed.and_then(number) {
        None => issues.push(SampleIssue {
            path: "elapsed_ns".to_owned(),
            problem: "missing or not an integer".to_owned(),
        }),
        Some(elapsed_ns) => {
            let seconds = elapsed_ns / NANOSECONDS_PER_SECOND;
            let tolerance = expected_seconds * ELAPSED_TOLERANCE;
            if (seconds - expected_seconds).abs() > tolerance {
                issues.push(SampleIssue {
                    path: "elapsed_ns".to_owned(),
                    problem: format!("{seconds:.3} s is not within 20% of {expected_seconds:.3} s"),
                });
            }
        }
    }

    if record
        .and_then(|record| record.get("timestamp"))
        .and_then(|value| value.as_date())
        .is_none()
    {
        issues.push(SampleIssue {
            path: "timestamp".to_owned(),
            problem: "missing or not a date".to_owned(),
        });
    }
    for path in ["kern_osversion", "hw_model"] {
        let present = record
            .and_then(|record| record.get(path))
            .and_then(|value| value.as_string())
            .is_some_and(|text| !text.is_empty());
        if !present {
            issues.push(SampleIssue {
                path: path.to_owned(),
                problem: "missing or not a string".to_owned(),
            });
        }
    }

    let power = plist_path(value, "processor.combined_power").and_then(number);
    match power {
        None => issues.push(SampleIssue {
            path: "processor.combined_power".to_owned(),
            problem: "missing or not a number".to_owned(),
        }),
        Some(power) if !(MINIMUM_POWER_MW..=MAXIMUM_POWER_MW).contains(&power) => {
            issues.push(SampleIssue {
                path: "processor.combined_power".to_owned(),
                problem: format!(
                    "{power:.0} mW is outside {MINIMUM_POWER_MW:.0} to {MAXIMUM_POWER_MW:.0} mW"
                ),
            });
        }
        Some(_) => {}
    }

    issues
}

/// Key paths whose presence or type differs from the known-good sample.
pub fn describe_differences(
    record: &plist::Value,
    known_good: Option<&plist::Value>,
) -> Vec<String> {
    let problems: Vec<(String, String)> = inspect_sample(record)
        .into_iter()
        .map(|issue| (issue.path, issue.problem))
        .collect();
    let mut lines = Vec::new();

    for key_path in REQUIRED_SAMPLE_PATHS {
        let current = plist_path(record, key_path);
        let expected = known_good.and_then(|known| plist_path(known, key_path));
        let problem = problems
            .iter()
            .find(|(path, _)| path == key_path)
            .map(|(_, problem)| problem.clone());
        if problem.is_none() && current.is_some() && value_kind(current) == value_kind(expected) {
            continue;
        }
        let current_text = problem.unwrap_or_else(|| describe_value(current));
        let expected_text = if known_good.is_some() {
            describe_value(expected)
        } else {
            "no known-good sample".to_owned()
        };
        lines.push(format!(
            "{key_path}: {current_text} (known-good {expected_text})"
        ));
    }

    lines
}

/// Reads a dotted path, returning `None` when any step is missing.
pub fn plist_path<'a>(value: &'a plist::Value, dotted_path: &str) -> Option<&'a plist::Value> {
    let mut current = value;
    for part in dotted_path.split('.') {
        current = current.as_dictionary()?.get(part)?;
    }
    Some(current)
}

/// The kind of a plist value, as the preflight compares shapes.
fn value_kind(value: Option<&plist::Value>) -> &'static str {
    match value {
        None => "missing",
        Some(plist::Value::Array(_)) => "array",
        Some(plist::Value::Dictionary(_)) => "dictionary",
        Some(plist::Value::Boolean(_)) => "boolean",
        Some(plist::Value::Real(_)) | Some(plist::Value::Integer(_)) => "number",
        Some(plist::Value::String(_)) => "string",
        Some(plist::Value::Date(_)) => "date",
        Some(plist::Value::Uid(_)) => "uid",
        _ => "unknown",
    }
}

fn describe_value(value: Option<&plist::Value>) -> String {
    match value {
        None => "missing".to_owned(),
        Some(plist::Value::Date(date)) => format!("date {}", date.to_xml_format()),
        Some(plist::Value::Array(_)) => "array".to_owned(),
        Some(plist::Value::Dictionary(_)) => "dictionary".to_owned(),
        Some(plist::Value::String(text)) => format!("string {text}"),
        Some(plist::Value::Boolean(value)) => format!("boolean {value}"),
        Some(plist::Value::Real(value)) => format!("number {value}"),
        Some(plist::Value::Integer(value)) => format!("number {value}"),
        Some(other) => format!("{other:?}"),
    }
}

fn capture(system: &System, argv: &[&str]) -> Result<String, String> {
    let argv: Vec<String> = argv.iter().map(|argument| (*argument).to_owned()).collect();
    let result = system.runner.capture(&argv, CaptureOptions::default())?;
    if !result.succeeded() {
        return Err(format!(
            "{} failed with exit code {}:\n{}",
            argv.join(" "),
            result.exit_code,
            result.stderr.trim()
        ));
    }
    Ok(result.stdout.trim().to_owned())
}

fn read_load_values(system: &System) -> Result<Vec<f64>, String> {
    let output = capture(system, &["sysctl", "-n", "vm.loadavg"])?;
    let values: Vec<f64> = output
        .split(|character: char| !(character.is_ascii_digit() || character == '.'))
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect();
    if values.len() < 3 {
        return Err(format!("Could not parse load averages from: {output}"));
    }
    Ok(values.into_iter().take(3).collect())
}

fn read_free_bytes(system: &System, target: &Path) -> Result<u64, String> {
    let output = capture(system, &["df", "-k", &target.to_string_lossy()])?;
    let line = output
        .lines()
        .nth(1)
        .ok_or_else(|| format!("df printed no data for {}", target.display()))?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    let free_kilobytes: u64 = fields
        .get(3)
        .and_then(|field| field.parse().ok())
        .ok_or_else(|| format!("Could not read free space from: {line}"))?;
    Ok(free_kilobytes * BYTES_PER_KILOBYTE)
}

fn read_busiest_processes(system: &System) -> Result<Vec<String>, String> {
    let output = capture(system, &["ps", "-Ao", "%cpu,comm", "-r"])?;
    Ok(output
        .lines()
        .skip(1)
        .take(BUSIEST_PROCESS_COUNT)
        .map(|line| {
            let trimmed = line.trim();
            let end = trimmed
                .char_indices()
                .nth(PROCESS_LINE_WIDTH)
                .map(|(index, _)| index)
                .unwrap_or(trimmed.len());
            trimmed[..end].to_owned()
        })
        .filter(|line| !line.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use crate::runner::{Captured, FakeRunner};
    use std::sync::Arc;

    /// The shipped sample plus the hardware model the preflight still requires.
    fn complete_sample() -> plist::Value {
        let text = KNOWN_GOOD.replace(
            "<key>kern_osversion</key>\n\t<string>24G517</string>",
            "<key>kern_osversion</key>\n\t<string>24G517</string>\n\t<key>hw_model</key>\n\t<string>Mac16,1</string>",
        );
        plist::Value::from_reader_xml(text.as_bytes()).expect("the sample parses")
    }

    #[test]
    fn the_known_good_sample_parses() {
        let known = known_good().expect("the shipped sample parses");
        assert_eq!(
            plist_path(&known, "kern_osversion").and_then(|value| value.as_string()),
            Some("24G517")
        );
        assert!(
            (plist_path(&known, "processor.combined_power")
                .and_then(number)
                .expect("the power is a number")
                - 120.996)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn a_complete_sample_has_no_preflight_issues() {
        assert_eq!(inspect_sample(&complete_sample()), []);
    }

    #[test]
    fn a_missing_combined_power_is_reported_against_the_known_good_sample() {
        let mut broken = complete_sample();
        let processor = broken
            .as_dictionary_mut()
            .expect("a dictionary")
            .get_mut("processor")
            .expect("a processor")
            .as_dictionary_mut()
            .expect("a dictionary");
        processor.remove("combined_power");

        let issues = inspect_sample(&broken);
        assert!(
            issues
                .iter()
                .any(|issue| issue.path == "processor.combined_power")
        );
        let differences = describe_differences(&broken, known_good().as_ref()).join("\n");
        assert!(differences.contains("processor.combined_power"));
    }

    #[test]
    fn implausible_power_and_elapsed_values_are_rejected() {
        let mut record = complete_sample();
        let dictionary = record.as_dictionary_mut().expect("a dictionary");
        let processor = dictionary
            .get_mut("processor")
            .expect("a processor")
            .as_dictionary_mut()
            .expect("a dictionary");
        processor.insert("combined_power".to_owned(), 250_000.0.into());
        dictionary.insert("elapsed_ns".to_owned(), 1_000_000_000.into());

        let paths: Vec<String> = inspect_sample(&record)
            .into_iter()
            .map(|issue| issue.path)
            .collect();
        assert!(paths.contains(&"processor.combined_power".to_owned()));
        assert!(paths.contains(&"elapsed_ns".to_owned()));
    }

    #[test]
    fn a_missing_field_is_explained_against_the_known_good_sample() {
        let mut broken = complete_sample();
        broken
            .as_dictionary_mut()
            .expect("a dictionary")
            .remove("hw_model");

        let lines = describe_differences(&broken, known_good().as_ref()).join("\n");
        assert!(lines.contains("hw_model"), "{lines}");
    }

    #[test]
    fn preflight_stops_below_two_gigabytes_free() {
        let runner = FakeRunner::new(|argv| {
            match argv.first().map(String::as_str) {
            Some("pmset") => Captured {
                exit_code: 0,
                stdout: "Now drawing from 'AC Power'".to_owned(),
                stderr: String::new(),
            },
            Some("sysctl") => Captured {
                exit_code: 0,
                stdout: "{ 1.00 2.00 3.00 }".to_owned(),
                stderr: String::new(),
            },
            Some("df") => Captured {
                exit_code: 0,
                stdout: "Filesystem 1024-blocks Used Available Capacity Mounted\n/dev/disk1 1000 500 100 10% /".to_owned(),
                stderr: String::new(),
            },
            Some("ps") => Captured {
                exit_code: 0,
                stdout: "%CPU COMM\n 12.0 node\n".to_owned(),
                stderr: String::new(),
            },
            _ => Captured {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        }
        });
        let system = System {
            runner: Arc::new(runner),
            clock: Arc::new(FakeClock::new(0)),
        };
        let mut printed = Vec::new();
        let error = run_preflight(
            &system,
            Path::new("/"),
            &mut |line| printed.push(line.to_owned()),
            false,
        )
        .expect_err("100 MB free is below the minimum");

        assert_eq!(error, "STOP: less than 2 GB free. Free some disk first.");
        assert!(
            printed
                .iter()
                .any(|line| line == "Power:  Now drawing from 'AC Power'")
        );
    }
}
