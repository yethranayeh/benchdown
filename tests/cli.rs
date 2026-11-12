//! The command line as a user sees it: output and exit status.

use std::process::{Command, Output};

fn benchdown(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_benchdown"))
        .args(arguments)
        .output()
        .expect("benchdown starts")
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
        "--rounds",
        "--idle",
        "--work",
        "--settle",
        "--seed",
        "--export-json",
        "--check",
    ] {
        assert!(help.contains(option), "{option} missing from run --help");
    }
}

#[test]
fn the_plan_shows_defaults() {
    let output = benchdown(&["run", "echo hi"]);
    assert!(output.status.success());
    let plan = text(&output.stdout);
    for line in [
        "Rounds:     5",
        "Idle block: 20s",
        "Work block: 20s",
        "Settle:     5s",
        "Seed:       20261106",
        "Total:      3m 45s for 5 blocks",
    ] {
        assert!(plan.contains(line), "{line} missing from the plan");
    }
}

#[test]
fn check_overrides_the_block_lengths_in_the_plan() {
    let output = benchdown(&["run", "--check", "echo hi"]);
    assert!(output.status.success());
    let plan = text(&output.stdout);
    assert!(plan.contains("Rounds:     1"), "{plan}");
    assert!(plan.contains("Idle block: 5s"), "{plan}");
    assert!(plan.contains("Work block: 5s"), "{plan}");
    assert!(plan.contains("Settle:     2s"), "{plan}");
    assert!(plan.contains("Total:      12s for 1 block"), "{plan}");
}

#[test]
fn the_plan_lists_each_rounds_order() {
    let output = benchdown(&["run", "-n", "a", "-n", "b", "one", "two"]);
    assert!(output.status.success());
    let plan = text(&output.stdout);
    for line in [
        "Round order",
        "  Round 1: b, a",
        "  Round 2: a, b",
        "  Round 3: a, b",
        "  Round 4: b, a",
        "  Round 5: a, b",
    ] {
        assert!(plan.contains(line), "{line} missing from the plan");
    }
}

#[test]
fn a_name_count_that_does_not_match_exits_two() {
    let output = benchdown(&["run", "-n", "only", "one", "two"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("command names"));
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
