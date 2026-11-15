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

/// The battery state recorded when the machine has no battery.
pub const NO_BATTERY: &str = "no battery";

/// The battery state recorded when a battery line has no readable percentage.
pub const UNKNOWN_BATTERY_STATE: &str = "unknown";

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
    pub battery: BatteryState,
    pub thermal: ThermalState,
    pub load_average: f64,
    pub free_bytes: u64,
}

/// What `pmset -g batt` said about the battery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatteryState {
    /// Whether the command printed a battery line at all.
    pub present: bool,
    /// Charge in percent, when the line carries one.
    pub percent: Option<u8>,
    /// The charging state after the percentage, or `no battery` on a desktop.
    pub state: String,
}

/// What `pmset -g therm` said about throttling.
///
/// Each field is `null` when the machine has recorded no value, including when
/// the command itself is missing or fails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThermalState {
    pub warning: Option<u32>,
    pub performance_warning: Option<u32>,
    pub cpu_speed_limit: Option<u32>,
    pub cpu_power_limit: Option<u32>,
}

impl ThermalState {
    /// The state recorded when the machine reports nothing.
    fn none() -> Self {
        Self {
            warning: None,
            performance_warning: None,
            cpu_speed_limit: None,
            cpu_power_limit: None,
        }
    }
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

/// Reads the power, battery and thermal state, prints the machine state, stops
/// on low disk and makes sure sudo has a ticket.
pub fn run_preflight(
    system: &System,
    free_disk_path: &Path,
    write: &mut dyn FnMut(&str),
    stdin_is_terminal: bool,
    needs_sudo: bool,
) -> Result<PreflightReport, String> {
    let battery_output = capture(system, &["pmset", "-g", "batt"])?;
    let power_source = battery_output
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_owned();
    let battery = parse_battery(&battery_output);
    let thermal = read_thermal(system);
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

    if needs_sudo {
        ensure_sudo_ticket(system, stdin_is_terminal)?;
    }
    Ok(PreflightReport {
        power_source,
        battery,
        thermal,
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

/// Parses the battery half of the report from `pmset -g batt` output.
///
/// The first line names the power source and each following non-empty line is
/// one battery. A Mac with no battery prints no such line.
pub fn parse_battery(output: &str) -> BatteryState {
    let Some(line) = output.lines().skip(1).find(|line| !line.trim().is_empty()) else {
        return BatteryState {
            present: false,
            percent: None,
            state: NO_BATTERY.to_owned(),
        };
    };
    let Some((before_percent, after_percent)) = line.split_once('%') else {
        return BatteryState {
            present: true,
            percent: None,
            state: UNKNOWN_BATTERY_STATE.to_owned(),
        };
    };

    BatteryState {
        present: true,
        percent: trailing_percent(before_percent),
        state: charging_state(after_percent),
    }
}

/// The digits that end the text before `%`, such as the `82` in
/// ` -InternalBattery-0 (id=22085731)\t82`.
fn trailing_percent(text: &str) -> Option<u8> {
    text.trim_end()
        .rsplit(|character: char| !character.is_ascii_digit())
        .next()
        .and_then(|digits| digits.parse().ok())
}

/// The charging state after the percentage, without the `present: true` marker.
fn charging_state(text: &str) -> String {
    let state = text.trim_start_matches([';', ' ']).trim();
    let state = state.strip_suffix("present: true").unwrap_or(state).trim();
    if state.is_empty() {
        UNKNOWN_BATTERY_STATE.to_owned()
    } else {
        state.to_owned()
    }
}

/// Parses `pmset -g therm` output.
///
/// A machine that has recorded no warning prints `Note: No thermal warning
/// level has been recorded` in place of a level, and one without CPU power
/// status prints no limit lines. Both leave the corresponding field `null`.
pub fn parse_thermal(output: &str) -> ThermalState {
    let mut thermal = ThermalState::none();
    for line in output.lines() {
        let line = line.to_ascii_lowercase();
        if line.contains("no thermal warning level") {
            thermal.warning = None;
        } else if let Some(value) = value_after(&line, "thermal warning level") {
            thermal.warning = Some(value);
        }
        if line.contains("no performance warning level") {
            thermal.performance_warning = None;
        } else if let Some(value) = value_after(&line, "performance warning level") {
            thermal.performance_warning = Some(value);
        }
        if let Some(value) = value_after(&line, "cpu_speed_limit") {
            thermal.cpu_speed_limit = Some(value);
        }
        if let Some(value) = value_after(&line, "cpu_power_limit") {
            thermal.cpu_power_limit = Some(value);
        }
    }
    thermal
}

/// The number after `name =` in one lower-cased line, when the line has it.
fn value_after(line: &str, name: &str) -> Option<u32> {
    let (_, rest) = line.split_once(name)?;
    let rest = rest.trim_start().strip_prefix('=')?;
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Reads the thermal state, never failing the preflight for it.
fn read_thermal(system: &System) -> ThermalState {
    let arguments = ["pmset", "-g", "therm"].map(str::to_owned);
    match system.runner.capture(&arguments, CaptureOptions::default()) {
        Ok(captured) if captured.succeeded() => parse_thermal(&captured.stdout),
        _ => ThermalState::none(),
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
    use crate::runner::{Captured, FakeRunner, Runner};
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
            true,
        )
        .expect_err("100 MB free is below the minimum");

        assert_eq!(error, "STOP: less than 2 GB free. Free some disk first.");
        assert!(
            printed
                .iter()
                .any(|line| line == "Power:  Now drawing from 'AC Power'")
        );
    }

    fn captured(exit_code: i32, stdout: &str) -> Captured {
        Captured {
            exit_code,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    /// A machine whose two `pmset` queries answer from the given outputs.
    fn preflight_system(
        battery: &str,
        thermal: &str,
        thermal_exit: i32,
    ) -> (System, Arc<FakeRunner>) {
        let battery = battery.to_owned();
        let thermal = thermal.to_owned();
        let runner = Arc::new(FakeRunner::new(move |argv| {
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            match argv.as_slice() {
                ["pmset", "-g", "batt"] => captured(0, &battery),
                ["pmset", "-g", "therm"] => captured(thermal_exit, &thermal),
                ["sysctl", ..] => captured(0, "{ 1.00 2.00 3.00 }"),
                ["df", ..] => captured(
                    0,
                    "Filesystem 1024-blocks Used Available Capacity Mounted\n\
                     /dev/disk1 1000 500 4000000 10% /",
                ),
                ["ps", ..] => captured(0, "%CPU COMM\n 12.0 node\n"),
                _ => captured(0, ""),
            }
        }));
        let system = System {
            runner: Arc::clone(&runner) as Arc<dyn Runner>,
            clock: Arc::new(FakeClock::new(0)),
        };
        (system, runner)
    }

    #[test]
    fn preflight_reads_battery_and_thermal_state_once() {
        let battery = "Now drawing from 'AC Power'\n\
             -InternalBattery-0 (id=22085731)\t82%; AC attached; not charging present: true\n";
        let thermal = "Note: No thermal warning level has been recorded\n\
             Note: No performance warning level has been recorded\n\
             CPU_Speed_Limit              = 100\n\
             CPU_Power_Limit              = 100\n";
        let (system, runner) = preflight_system(battery, thermal, 0);
        let mut printed = Vec::new();

        let report = run_preflight(
            &system,
            Path::new("/"),
            &mut |line| printed.push(line.to_owned()),
            false,
            true,
        )
        .expect("the machine passes the preflight");

        assert_eq!(report.power_source, "Now drawing from 'AC Power'");
        assert_eq!(
            report.battery,
            BatteryState {
                present: true,
                percent: Some(82),
                state: "AC attached; not charging".to_owned(),
            }
        );
        assert_eq!(report.thermal.cpu_speed_limit, Some(100));
        assert_eq!(report.thermal.cpu_power_limit, Some(100));
        assert_eq!(report.thermal.warning, None);

        let pmset_calls: Vec<Vec<String>> = runner
            .argv()
            .into_iter()
            .filter(|argv| argv.first().map(String::as_str) == Some("pmset"))
            .collect();
        assert_eq!(
            pmset_calls,
            [["pmset", "-g", "batt"], ["pmset", "-g", "therm"]]
                .map(|argv| argv.map(str::to_owned).to_vec()),
            "battery output is read once and reused for the power source"
        );
    }

    #[test]
    fn a_failing_thermal_command_leaves_the_thermal_fields_empty() {
        let battery = "Now drawing from 'Battery Power'\n\
             -InternalBattery-0 (id=7)\t50%; discharging; 2:00 remaining present: true\n";
        let (system, _) = preflight_system(battery, "thermal status unavailable", 1);
        let mut printed = Vec::new();

        let report = run_preflight(
            &system,
            Path::new("/"),
            &mut |line| printed.push(line.to_owned()),
            false,
            true,
        )
        .expect("a failing thermal query does not stop the preflight");

        assert_eq!(report.thermal, ThermalState::none());
        assert_eq!(report.battery.percent, Some(50));
        assert_eq!(report.battery.state, "discharging; 2:00 remaining");
    }

    #[test]
    fn a_charging_laptop_reports_its_charge_and_state() {
        let battery = parse_battery(
            "Now drawing from 'AC Power'\n\
             -InternalBattery-0 (id=4653155)\t76%; charging; 0:31 remaining present: true\n",
        );
        assert_eq!(
            battery,
            BatteryState {
                present: true,
                percent: Some(76),
                state: "charging; 0:31 remaining".to_owned(),
            }
        );
    }

    #[test]
    fn a_laptop_held_on_ac_reports_its_state() {
        let battery = parse_battery(
            "Now drawing from 'AC Power'\n\
             -InternalBattery-0 (id=22085731)\t94%; AC attached; not charging present: true\n",
        );
        assert_eq!(
            battery,
            BatteryState {
                present: true,
                percent: Some(94),
                state: "AC attached; not charging".to_owned(),
            }
        );
    }

    #[test]
    fn a_desktop_without_a_battery_says_so() {
        let battery = parse_battery("Now drawing from 'AC Power'\n");
        assert_eq!(
            battery,
            BatteryState {
                present: false,
                percent: None,
                state: NO_BATTERY.to_owned(),
            }
        );
    }

    #[test]
    fn a_battery_line_without_a_percentage_is_unknown() {
        let battery = parse_battery(
            "Now drawing from 'AC Power'\n -InternalBattery-0 (id=7) present: true\n",
        );
        assert_eq!(
            battery,
            BatteryState {
                present: true,
                percent: None,
                state: UNKNOWN_BATTERY_STATE.to_owned(),
            }
        );
    }

    #[test]
    fn thermal_limits_are_read_when_the_machine_reports_them() {
        let thermal = parse_thermal(
            "Note: No thermal warning level has been recorded\n\
             Note: No performance warning level has been recorded\n\
             CPU_Speed_Limit              = 100\n\
             CPU_Power_Limit              = 100\n",
        );
        assert_eq!(
            thermal,
            ThermalState {
                warning: None,
                performance_warning: None,
                cpu_speed_limit: Some(100),
                cpu_power_limit: Some(100),
            }
        );
    }

    #[test]
    fn thermal_notes_become_empty_fields() {
        let thermal = parse_thermal(
            "Note: No thermal warning level has been recorded\n\
             Note: No performance warning level has been recorded\n\
             Note: No CPU power status has been recorded\n",
        );
        assert_eq!(thermal, ThermalState::none());
    }

    #[test]
    fn recorded_thermal_warnings_are_read() {
        let thermal = parse_thermal(
            "2026-11-14 13:10:40 +0100 Thermal Warning Level = 1\n\
             2026-11-14 13:10:40 +0100 Performance Warning Level = 2\n\
             CPU_Speed_Limit = 89\n",
        );
        assert_eq!(thermal.warning, Some(1));
        assert_eq!(thermal.performance_warning, Some(2));
        assert_eq!(thermal.cpu_speed_limit, Some(89));
        assert_eq!(thermal.cpu_power_limit, None);
    }

    #[test]
    fn a_thermal_line_with_several_values_is_read() {
        let thermal = parse_thermal(
            "2020-11-24 13:10:40 +0100 CPU Power notify \
             CPU_Scheduler_Limit = 100 CPU_Available_CPUs = 8 CPU_Speed_Limit = 35\n",
        );
        assert_eq!(thermal.cpu_speed_limit, Some(35));
        assert_eq!(thermal.warning, None);
    }

    #[test]
    fn the_report_gains_battery_and_thermal_objects_beside_the_power_source() {
        let report = PreflightReport {
            power_source: "Now drawing from 'AC Power'".to_owned(),
            battery: BatteryState {
                present: true,
                percent: Some(94),
                state: "AC attached; not charging".to_owned(),
            },
            thermal: ThermalState::none(),
            load_average: 1.69,
            free_bytes: 5_212_016_640,
        };

        let json = serde_json::to_value(&report).expect("the report serializes");
        assert_eq!(json["powerSource"], "Now drawing from 'AC Power'");
        assert_eq!(json["battery"]["present"], true);
        assert_eq!(json["battery"]["percent"], 94);
        assert_eq!(json["battery"]["state"], "AC attached; not charging");
        assert!(json["thermal"]["warning"].is_null());
        assert!(json["thermal"]["performanceWarning"].is_null());
        assert!(json["thermal"]["cpuSpeedLimit"].is_null());
        assert!(json["thermal"]["cpuPowerLimit"].is_null());
        assert_eq!(json["loadAverage"], 1.69);
        assert_eq!(json["freeBytes"], 5_212_016_640u64);
    }
}
