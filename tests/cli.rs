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
