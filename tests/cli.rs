//! The command line as a user sees it: output and exit status.

use std::process::{Command, Output};

fn benchdown(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_benchdown"))
        .args(arguments)
        .output()
        .expect("benchdown starts")
}

fn benchdown_with_env(arguments: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_benchdown"));
    command.args(arguments);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("benchdown starts")
}

fn temporary(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "benchdown-cli-{}-{serial}-{name}",
        std::process::id()
    ))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("output is UTF-8")
}

#[test]
fn version_prints_the_crate_version() {
    let output = benchdown(&["--version"]);
    assert!(output.status.success());
    assert_eq!(
        text(&output.stdout).trim(),
        format!("benchdown {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn help_lists_the_run_command() {
    let output = benchdown(&["--help"]);
    assert!(output.status.success());
    assert!(text(&output.stdout).contains("run"));
}

#[test]
fn run_help_lists_every_option() {
    let output = benchdown(&["run", "--help"]);
    assert!(output.status.success());
    let help = text(&output.stdout);
    for option in [
        "--command-name",
        "--shell",
        "--prepare",
        "--cwd",
        "--env",
        "--env-clear",
        "--sensor",
        "--rounds",
        "--idle",
        "--work",
        "--settle",
        "--seed",
        "--export-json",
        "--out",
        "--check",
    ] {
        assert!(help.contains(option), "{option} missing from run --help");
    }
}

#[test]
fn a_name_count_that_does_not_match_exits_two() {
    let output = benchdown(&["run", "-n", "only", "one", "two"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("command names"));
}

#[test]
fn an_unterminated_quote_exits_two_before_the_run() {
    let output = benchdown(&["run", "-N", "echo 'unterminated"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("Usage:"), "{stderr}");
    assert!(stderr.contains("echo 'unterminated"), "{stderr}");

    let stdout = text(&output.stdout);
    assert!(!stdout.contains("Results:"), "{stdout}");
    assert!(!stdout.contains("Plan:"), "{stdout}");
}

#[test]
fn an_env_without_a_value_exits_two() {
    let output = benchdown(&["run", "--env", "NOVALUE", "true"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("KEY=VALUE"));
}

#[test]
fn no_command_exits_two() {
    let output = benchdown(&["run"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("COMMAND"));
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn analyze_prints_the_table() {
    let output = benchdown(&["analyze", &fixture("power.plist"), &fixture("blocks.tsv")]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let table = text(&output.stdout);

    for line in [
        "samples: 12, median interval 1.000 s",
        "idle power: median 115 mW, range 115 to 115 mW",
        "dropped blocks: none",
        "command\truns/block\tmedian mJ\tmin mJ\tmax mJ\tmedian uWh\tspread\tverdict\tclean median mJ\tclean rounds\tclean verdict",
        // 160 mJ net over 120 runs is 1.3 mJ each; 1.3 mJ is 0.4 uWh; one clean
        // round is below the four a workload needs.
        "one\t120\t1.3\t1.3\t1.3\t0.4\t0%\tok\t1.3\t1\trerun",
    ] {
        assert!(table.contains(line), "{line} missing from:\n{table}");
    }
}

#[test]
fn analyze_json_prints_the_analysis() {
    let output = benchdown(&[
        "analyze",
        "--json",
        &fixture("power.plist"),
        &fixture("blocks.tsv"),
    ]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let analysis: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is JSON");

    assert_eq!(analysis["sampleCount"], 12);
    assert_eq!(analysis["blocks"].as_array().expect("blocks").len(), 2);
    assert_eq!(analysis["workloads"][0]["label"], "one");
    assert_eq!(analysis["workloads"][0]["cleanVerdict"], "rerun");
    assert_eq!(
        analysis["droppedRounds"].as_array().expect("rounds").len(),
        0
    );
}

#[test]
fn analyze_refuses_a_missing_file_without_a_usage_line() {
    let output = benchdown(&["analyze", "no-such-power.plist", &fixture("blocks.tsv")]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("no-such-power.plist"), "{stderr}");
    assert!(!stderr.contains("Usage:"), "{stderr}");
}

const SESSION_HEADER: &str = "time\tload1\tload5\tload15\tcpu_percent\trss_kb\tpid\tcommand";

#[test]
fn session_help_lists_every_option() {
    let output = benchdown(&["session", "--help"]);
    assert!(output.status.success());
    let help = text(&output.stdout);
    for option in [
        "--spotlight-off",
        "--time-machine-off",
        "--wifi-off",
        "--jobs",
        "--out",
        "--sample-seconds",
        "--stop-on-failure",
        "--dry-run",
        "restore",
    ] {
        assert!(
            help.contains(option),
            "{option} missing from session --help"
        );
    }
}

#[test]
fn session_without_jobs_or_a_command_exits_two() {
    let output = benchdown(&["session"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("--jobs"));
}

#[test]
fn session_refuses_jobs_and_a_trailing_command_together() {
    let output = benchdown(&["session", "--jobs", "jobs.json", "--", "echo", "hi"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("--jobs and --"), "{stderr}");
}

#[test]
fn session_refuses_a_malformed_jobs_file() {
    let path = temporary("jobs.json");
    std::fs::write(&path, "[").expect("the jobs file writes");
    let output = benchdown(&[
        "session",
        "--jobs",
        path.to_str().expect("the path is UTF-8"),
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("jobs file"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_session_dry_run_prints_the_plan_and_writes_nothing() {
    let jobs = temporary("dry-jobs.json");
    std::fs::write(
        &jobs,
        r#"[{"name":"hello","command":"echo hello","estimateMinutes":1}]"#,
    )
    .expect("the jobs file writes");
    let out = temporary("dry-out");
    let state = temporary("dry-state.json");
    let output = benchdown_with_env(
        &[
            "session",
            "--dry-run",
            "--jobs",
            jobs.to_str().expect("the path is UTF-8"),
            "--out",
            out.to_str().expect("the path is UTF-8"),
        ],
        &[(
            "BENCHDOWN_SESSION_STATE_FILE",
            state.to_str().expect("the path is UTF-8"),
        )],
    );
    let stdout = text(&output.stdout);
    assert!(output.status.success(), "{stdout}{}", text(&output.stderr));
    assert!(stdout.contains("hello"), "{stdout}");
    assert!(stdout.contains("echo hello"), "{stdout}");
    assert!(stdout.contains("Dry run"), "{stdout}");
    assert!(stdout.contains("benchdown session restore"), "{stdout}");
    assert!(!out.exists(), "a dry run writes no session folder");
    assert!(!state.exists(), "a dry run writes no state file");
    let _ = std::fs::remove_file(&jobs);
}

#[test]
fn session_restore_without_a_state_file_exits_zero() {
    let state = temporary("missing-state.json");
    let _ = std::fs::remove_file(&state);
    let output = benchdown_with_env(
        &["session", "restore"],
        &[(
            "BENCHDOWN_SESSION_STATE_FILE",
            state.to_str().expect("the path is UTF-8"),
        )],
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(text(&output.stdout).contains("No restore state file"));
}

#[test]
fn analyze_takes_session_samples_and_drops_the_round() {
    let samples = temporary("samples.tsv");
    std::fs::write(
        &samples,
        format!(
            "{SESSION_HEADER}\n2026-11-08T10:00:05Z\t4.4\t2.0\t1.5\t1.0\t512\t101\t/usr/sbin/mds\n"
        ),
    )
    .expect("the samples file writes");

    let output = benchdown(&[
        "analyze",
        "--json",
        &fixture("power.plist"),
        &fixture("blocks.tsv"),
        samples.to_str().expect("the path is UTF-8"),
    ]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let analysis: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is JSON");
    assert_eq!(
        analysis["contaminatedBlocks"]
            .as_array()
            .expect("blocks")
            .len(),
        1
    );
    assert_eq!(analysis["contaminatedBlocks"][0]["reason"], "load");
    assert_eq!(analysis["droppedRounds"], serde_json::json!([1]));
    assert_eq!(analysis["workloads"][0]["cleanRounds"], 0);
    let _ = std::fs::remove_file(&samples);
}

#[test]
fn analyze_refuses_an_unreadable_session_samples_file() {
    let output = benchdown(&[
        "analyze",
        &fixture("power.plist"),
        &fixture("blocks.tsv"),
        "no-such-samples.tsv",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("no-such-samples.tsv"), "{stderr}");
}

#[test]
fn run_refuses_an_unreadable_session_samples_file_before_sampling() {
    let missing = temporary("missing-samples.tsv");
    let _ = std::fs::remove_file(&missing);
    let output = benchdown_with_env(
        &["run", "--check", "-N", "true"],
        &[(
            "BENCHDOWN_SESSION_SAMPLES",
            missing.to_str().expect("the path is UTF-8"),
        )],
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("BENCHDOWN_SESSION_SAMPLES"), "{stderr}");
    assert!(stderr.contains("missing-samples.tsv"), "{stderr}");
    let stdout = text(&output.stdout);
    assert!(!stdout.contains("Results:"), "{stdout}");
    assert!(!stdout.contains("Plan:"), "{stdout}");
}

fn reference_directory(variable: &str) -> Option<std::path::PathBuf> {
    let Some(directory) = std::env::var_os(variable) else {
        eprintln!(
            "skipping {variable}: set it to a directory holding power.plist, \
             blocks.tsv and analysis.json"
        );
        return None;
    };
    Some(std::path::PathBuf::from(directory))
}

fn read_json(path: &std::path::Path) -> Option<serde_json::Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("skipping: {} could not be read ({error})", path.display());
            return None;
        }
    };
    Some(serde_json::from_str(&text).expect("the recorded analysis is JSON"))
}

/// Compares two analyses: keys, strings and integers must match exactly, floats
/// within a relative tolerance of 1e-9.
fn same_analysis(
    ours: &serde_json::Value,
    recorded: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    use serde_json::Value;

    match (ours, recorded) {
        (Value::Object(ours), Value::Object(recorded)) => {
            for (key, value) in recorded {
                match ours.get(key) {
                    Some(mine) => same_analysis(mine, value, &format!("{path}.{key}"))?,
                    None => return Err(format!("{path}.{key} is missing")),
                }
            }
            for key in ours.keys() {
                if !recorded.contains_key(key) {
                    return Err(format!("{path}.{key} is extra"));
                }
            }
            Ok(())
        }
        (Value::Array(ours), Value::Array(recorded)) => {
            if ours.len() != recorded.len() {
                return Err(format!(
                    "{path} holds {} entries, recorded {}",
                    ours.len(),
                    recorded.len()
                ));
            }
            for (index, (mine, theirs)) in ours.iter().zip(recorded).enumerate() {
                same_analysis(mine, theirs, &format!("{path}[{index}]"))?;
            }
            Ok(())
        }
        (Value::Number(ours), Value::Number(recorded)) => {
            match (ours.as_f64(), recorded.as_f64()) {
                (Some(ours), Some(recorded)) => {
                    let scale = ours.abs().max(recorded.abs()).max(f64::MIN_POSITIVE);
                    if (ours - recorded).abs() / scale > 1e-9 {
                        return Err(format!("{path} is {ours}, recorded {recorded}"));
                    }
                    Ok(())
                }
                _ => Err(format!("{path} is not a number")),
            }
        }
        _ if ours == recorded => Ok(()),
        _ => Err(format!("{path} is {ours}, recorded {recorded}")),
    }
}

#[test]
#[ignore = "reads the recorded runs named by BENCHDOWN_REFERENCE_A and BENCHDOWN_REFERENCE_B"]
fn recorded_runs_match_their_analysis() {
    for variable in ["BENCHDOWN_REFERENCE_A", "BENCHDOWN_REFERENCE_B"] {
        let Some(directory) = reference_directory(variable) else {
            continue;
        };
        let power = directory.join("power.plist");
        let blocks = directory.join("blocks.tsv");
        let Some(recorded) = read_json(&directory.join("analysis.json")) else {
            continue;
        };

        let output = benchdown(&[
            "analyze",
            "--json",
            power.to_str().expect("the path is UTF-8"),
            blocks.to_str().expect("the path is UTF-8"),
        ]);
        assert!(output.status.success(), "{}", text(&output.stderr));
        let ours: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("stdout is JSON");

        same_analysis(&ours, &recorded, variable).unwrap_or_else(|problem| panic!("{problem}"));
        println!("{variable}: matches the recorded analysis");
    }
}

#[test]
#[ignore = "reads the recorded run named by BENCHDOWN_REFERENCE_A"]
fn recorded_run_a_table_matches() {
    let Some(directory) = reference_directory("BENCHDOWN_REFERENCE_A") else {
        return;
    };
    let power = directory.join("power.plist");
    let blocks = directory.join("blocks.tsv");

    let output = benchdown(&[
        "analyze",
        power.to_str().expect("the path is UTF-8"),
        blocks.to_str().expect("the path is UTF-8"),
    ]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let table = text(&output.stdout);

    for row in [
        "node\t539\t204.6",
        "list@100\t171\t748.5",
        "list-json@100\t171\t732.4",
        "view@100\t179\t728.7",
        "create@100\t131\t850.8",
        "list@10000\t6\t20105.9",
    ] {
        assert!(table.contains(row), "{row} missing from:\n{table}");
    }
    assert!(table.contains("dropped blocks: none"), "{table}");
}

#[test]
fn a_sensor_this_platform_lacks_exits_two_with_the_names_it_has() {
    let missing = if cfg!(target_os = "macos") {
        "rapl"
    } else {
        "smc"
    };
    let output = benchdown(&["run", "--sensor", missing, "true"]);
    assert_eq!(output.status.code(), Some(2));
    let message = text(&output.stderr);
    assert!(message.contains("is not available on"), "{message}");
    assert!(message.contains(missing), "{message}");
}

#[test]
fn an_unknown_sensor_exits_two_and_lists_the_choices() {
    let output = benchdown(&["run", "--sensor", "gpu", "true"]);
    assert_eq!(output.status.code(), Some(2));
    let message = text(&output.stderr);
    assert!(message.contains("unknown sensor gpu"), "{message}");
    assert!(message.contains("powermetrics, smc, rapl"), "{message}");
}
