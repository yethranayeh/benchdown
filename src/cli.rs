//! The `benchdown` command line.

use std::path::PathBuf;

use clap::{Args as ClapArgs, CommandFactory, Parser, Subcommand, error::ErrorKind};

use crate::analysis;
use crate::blocks;
use crate::samples;

/// Defaults follow Zinn's measurement harness, which benchdown's method comes from.
pub const DEFAULT_ROUNDS: u64 = 5;
pub const DEFAULT_IDLE_SECONDS: u64 = 20;
pub const DEFAULT_WORK_SECONDS: u64 = 20;
pub const DEFAULT_SETTLE_SECONDS: u64 = 5;

/// Fixed so every run visits the workloads in the same order.
pub const SCHEDULE_SEED: u32 = 20_261_106;

/// `--check` keeps every block short enough to be a smoke run.
pub const CHECK_ROUNDS: u64 = 1;
pub const CHECK_IDLE_SECONDS: u64 = 5;
pub const CHECK_WORK_SECONDS: u64 = 5;
pub const CHECK_SETTLE_SECONDS: u64 = 2;

/// The whole command line.
#[derive(Debug, Parser)]
#[command(
    name = "benchdown",
    version,
    about = "Measure a command's processor energy the way hyperfine measures its time",
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Args {
    #[command(subcommand)]
    pub command: ArgsCommand,
}

#[derive(Debug, Subcommand)]
pub enum ArgsCommand {
    /// Plan and run measured work blocks for one or more commands
    Run(RunArgs),
    /// Report energy per run from a recorded power log and block log
    Analyze(AnalyzeArgs),
}

/// Everything `benchdown analyze` accepts.
#[derive(Debug, ClapArgs)]
pub struct AnalyzeArgs {
    /// Power log written by powermetrics with -f plist
    #[arg(value_name = "POWER")]
    pub power: PathBuf,

    /// Block log written by benchdown run
    #[arg(value_name = "BLOCKS")]
    pub blocks: PathBuf,

    /// Print the analysis as JSON instead of a table
    #[arg(long)]
    pub json: bool,
}

impl AnalyzeArgs {
    /// Reads both logs, analyzes them and prints the result.
    pub fn report(self) -> Result<(), String> {
        let power = read(&self.power)?;
        let block_log = read(&self.blocks)?;
        let samples = samples::parse_power_log(&power);
        let blocks = blocks::parse_blocks(&block_log)?;
        let analysis = analysis::analyze(&samples, &blocks, &[])?;

        if self.json {
            let json =
                serde_json::to_string_pretty(&analysis).map_err(|error| error.to_string())?;
            println!("{json}");
        } else {
            println!("{}", analysis::format_analysis(&analysis).join("\n"));
        }
        Ok(())
    }
}

fn read(path: &std::path::Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Everything `benchdown run` accepts, as typed.
#[derive(Debug, ClapArgs)]
pub struct RunArgs {
    /// One or more commands, each a single shell string
    #[arg(value_name = "COMMAND", required = true)]
    pub commands: Vec<String>,

    /// Name for one command, in order; repeat once per command
    #[arg(short = 'n', long = "command-name", value_name = "N")]
    pub command_names: Vec<String>,

    /// Shell that runs each command; `none` splits it on whitespace and runs it directly
    #[arg(
        short = 'S',
        long,
        value_name = "SHELL",
        default_value = "sh",
        conflicts_with = "no_shell"
    )]
    pub shell: String,

    /// Run commands without a shell; same as --shell=none
    #[arg(short = 'N')]
    pub no_shell: bool,

    /// Command to run before each work block; one per command, or one for all
    #[arg(short = 'p', long, value_name = "CMD")]
    pub prepare: Vec<String>,

    /// Rounds over the workloads, at most 10000
    #[arg(
        long,
        value_name = "N",
        default_value_t = DEFAULT_ROUNDS,
        value_parser = clap::value_parser!(u64).range(1..=10_000)
    )]
    pub rounds: u64,

    /// Idle block length in whole seconds
    #[arg(
        long,
        value_name = "S",
        default_value_t = DEFAULT_IDLE_SECONDS,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub idle: u64,

    /// Work block length in whole seconds
    #[arg(
        long,
        value_name = "S",
        default_value_t = DEFAULT_WORK_SECONDS,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub work: u64,

    /// Pause before each idle block, in whole seconds
    #[arg(long, value_name = "S", default_value_t = DEFAULT_SETTLE_SECONDS)]
    pub settle: u64,

    /// Seed for the shuffled round order
    #[arg(long, value_name = "N", default_value_t = SCHEDULE_SEED)]
    pub seed: u32,

    /// Write the results as hyperfine-shaped JSON
    #[arg(long, value_name = "F")]
    pub export_json: Option<PathBuf>,

    /// Folder for the run's files (default ./benchdown-<yyyymmdd-hhmmss>/)
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,

    /// Smoke run: one round of 5 s blocks with 2 s settle, overriding the block lengths
    #[arg(long)]
    pub check: bool,
}

/// How a command reaches the operating system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shell {
    /// Run the command through this shell.
    Program(String),
    /// Split the command on whitespace and run it directly.
    None,
}

/// One command to measure, with what it needs before a work block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workload {
    /// Shown in results; defaults to the command itself.
    pub name: String,
    pub command: String,
    pub prepare: Option<String>,
}

/// The resolved plan for one run: defaults applied and `--check` expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub workloads: Vec<Workload>,
    pub shell: Shell,
    pub rounds: u64,
    pub idle_seconds: u64,
    pub work_seconds: u64,
    pub settle_seconds: u64,
    pub seed: u32,
    pub export_json: Option<PathBuf>,
    pub out: Option<PathBuf>,
}

impl Plan {
    /// Resolves typed arguments into the run benchdown would execute.
    ///
    /// Fails with a message for a command line clap accepts but the method cannot use.
    pub fn new(args: RunArgs) -> Result<Self, String> {
        let count = args.commands.len();
        if !args.command_names.is_empty() && args.command_names.len() != count {
            return Err(format!(
                "{} command names given for {} commands: pass one name per command, or none",
                args.command_names.len(),
                count
            ));
        }
        if args.prepare.len() > 1 && args.prepare.len() != count {
            return Err(format!(
                "{} prepare commands given for {} commands: pass one per command, or one for all",
                args.prepare.len(),
                count
            ));
        }

        let shell = if args.no_shell || args.shell == "none" {
            Shell::None
        } else {
            Shell::Program(args.shell)
        };

        let workloads = args
            .commands
            .into_iter()
            .enumerate()
            .map(|(index, command)| Workload {
                name: args
                    .command_names
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| command.clone()),
                command,
                prepare: match args.prepare.len() {
                    0 => None,
                    1 => args.prepare.first().cloned(),
                    _ => args.prepare.get(index).cloned(),
                },
            })
            .collect();

        let (rounds, idle_seconds, work_seconds, settle_seconds) = if args.check {
            (
                CHECK_ROUNDS,
                CHECK_IDLE_SECONDS,
                CHECK_WORK_SECONDS,
                CHECK_SETTLE_SECONDS,
            )
        } else {
            (args.rounds, args.idle, args.work, args.settle)
        };

        Ok(Self {
            workloads,
            shell,
            rounds,
            idle_seconds,
            work_seconds,
            settle_seconds,
            seed: args.seed,
            export_json: args.export_json,
            out: args.out,
        })
    }

    /// Work blocks the run will perform.
    pub fn blocks(&self) -> u64 {
        self.workloads.len() as u64 * self.rounds
    }

    /// Seconds one workload spends in a round: settle, idle block, work block.
    pub fn seconds_per_block(&self) -> u64 {
        self.settle_seconds + self.idle_seconds + self.work_seconds
    }

    /// Seconds the whole run plans for.
    pub fn total_seconds(&self) -> u64 {
        self.blocks() * self.seconds_per_block()
    }
}

/// Prints a refused command line and exits with clap's usage-error status.
pub fn exit_with(message: &str) -> ! {
    let mut command = Args::command();
    let mut run = command
        .find_subcommand_mut("run")
        .expect("the run subcommand is declared")
        .clone()
        .bin_name("benchdown run");
    run.error(ErrorKind::InvalidValue, message.to_owned())
        .exit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(arguments: &[&str]) -> Result<Plan, String> {
        let mut argv = vec!["benchdown", "run"];
        argv.extend_from_slice(arguments);
        let parsed = match Args::try_parse_from(argv) {
            Ok(parsed) => parsed,
            Err(error) => return Err(error.to_string()),
        };
        match parsed.command {
            ArgsCommand::Run(run) => Plan::new(run),
            ArgsCommand::Analyze(_) => unreachable!("the helper always builds a run command"),
        }
    }

    #[test]
    fn defaults_come_from_the_measurement_method() {
        let plan = plan(&["echo hi"]).expect("one command plans a run");
        assert_eq!(plan.rounds, DEFAULT_ROUNDS);
        assert_eq!(plan.idle_seconds, DEFAULT_IDLE_SECONDS);
        assert_eq!(plan.work_seconds, DEFAULT_WORK_SECONDS);
        assert_eq!(plan.settle_seconds, DEFAULT_SETTLE_SECONDS);
        assert_eq!(plan.seed, SCHEDULE_SEED);
        assert_eq!(plan.shell, Shell::Program("sh".to_owned()));
        assert_eq!(plan.export_json, None);
        assert_eq!(plan.workloads.len(), 1);
        assert_eq!(plan.workloads[0].name, "echo hi");
        assert_eq!(plan.workloads[0].prepare, None);
    }

    #[test]
    fn every_option_reaches_the_plan() {
        let plan = plan(&[
            "-n",
            "first",
            "-n",
            "second",
            "-S",
            "bash",
            "-p",
            "make clean",
            "-p",
            "make",
            "--rounds",
            "2",
            "--idle",
            "3",
            "--work",
            "4",
            "--settle",
            "1",
            "--seed",
            "7",
            "--export-json",
            "results.json",
            "--out",
            "somewhere",
            "cargo build",
            "cargo test",
        ])
        .expect("every option parses");

        assert_eq!(plan.rounds, 2);
        assert_eq!(plan.idle_seconds, 3);
        assert_eq!(plan.work_seconds, 4);
        assert_eq!(plan.settle_seconds, 1);
        assert_eq!(plan.seed, 7);
        assert_eq!(plan.shell, Shell::Program("bash".to_owned()));
        assert_eq!(
            plan.export_json,
            Some(std::path::PathBuf::from("results.json"))
        );
        assert_eq!(plan.out, Some(std::path::PathBuf::from("somewhere")));
        assert_eq!(plan.workloads.len(), 2);
        assert_eq!(plan.workloads[0].name, "first");
        assert_eq!(plan.workloads[0].prepare.as_deref(), Some("make clean"));
        assert_eq!(plan.workloads[1].name, "second");
        assert_eq!(plan.workloads[1].prepare.as_deref(), Some("make"));
    }

    #[test]
    fn check_overrides_the_block_lengths() {
        let plan = plan(&["--check", "--rounds", "9", "--idle", "9", "echo hi"])
            .expect("--check plans a smoke run");
        assert_eq!(plan.rounds, CHECK_ROUNDS);
        assert_eq!(plan.idle_seconds, CHECK_IDLE_SECONDS);
        assert_eq!(plan.work_seconds, CHECK_WORK_SECONDS);
        assert_eq!(plan.settle_seconds, CHECK_SETTLE_SECONDS);
    }

    #[test]
    fn shell_none_runs_commands_directly() {
        let spelled_out = plan(&["-S", "none", "echo hi"]).expect("-S none parses");
        assert_eq!(spelled_out.shell, Shell::None);
        let short = plan(&["-N", "echo hi"]).expect("-N parses");
        assert_eq!(short.shell, Shell::None);
    }

    #[test]
    fn a_name_count_that_does_not_match_fails() {
        let error = plan(&["-n", "only", "one", "two"]).expect_err("counts differ");
        assert!(error.contains("command names"), "{error}");
        let error = plan(&["-n", "a", "-n", "b", "-n", "c", "one"]).expect_err("counts differ");
        assert!(error.contains("command names"), "{error}");
    }

    #[test]
    fn one_prepare_command_applies_to_every_command() {
        let plan = plan(&["-p", "true", "one", "two"]).expect("one prepare for all");
        assert_eq!(plan.workloads[0].prepare.as_deref(), Some("true"));
        assert_eq!(plan.workloads[1].prepare.as_deref(), Some("true"));
    }

    #[test]
    fn a_prepare_count_that_matches_neither_rule_fails() {
        let error = plan(&["-p", "a", "-p", "b", "one", "two", "three"])
            .expect_err("three commands take one prepare or three");
        assert!(error.contains("prepare commands"), "{error}");
    }

    #[test]
    fn no_command_is_a_usage_error() {
        let error = plan(&[]).expect_err("a run needs a command");
        assert!(error.contains("COMMAND"), "{error}");
    }

    #[test]
    fn the_total_is_every_block_with_its_settle() {
        let plan = plan(&["one", "two"]).expect("two commands plan");
        assert_eq!(plan.blocks(), 10);
        assert_eq!(plan.seconds_per_block(), 45);
        assert_eq!(plan.total_seconds(), 450);
    }

    #[test]
    fn rounds_are_capped_so_a_plan_stays_printable() {
        let error = plan(&["--rounds", "20000", "one"]).expect_err("20000 rounds is refused");
        assert!(error.contains("20000"), "{error}");
    }
}
