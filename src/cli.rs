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
    /// Prepare the Mac, run jobs, and restore every changed setting
    Session(SessionArgs),
}

/// Everything `benchdown session` accepts.
#[derive(Debug, ClapArgs)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub action: Option<SessionAction>,

    /// Turn Spotlight indexing off for the session
    #[arg(long)]
    pub spotlight_off: bool,

    /// Turn automatic Time Machine backups off for the session
    #[arg(long)]
    pub time_machine_off: bool,

    /// Turn the Wi-Fi interface off for the session
    #[arg(long)]
    pub wifi_off: bool,

    /// JSON file with an array of {name, command, estimateMinutes}
    #[arg(long, value_name = "FILE")]
    pub jobs: Option<PathBuf>,

    /// Session output folder (default ./benchdown-sessions)
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,

    /// Seconds between load and process samples
    #[arg(
        long,
        value_name = "N",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub sample_seconds: Option<u64>,

    /// Stop after the first failed job
    #[arg(long)]
    pub stop_on_failure: bool,

    /// Print the plan without changing anything or asking for a password
    #[arg(long)]
    pub dry_run: bool,

    /// The command to run, after --
    #[arg(last = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum SessionAction {
    /// Undo a session that died before it could restore its changes
    Restore,
}

/// What `benchdown session` will do after its arguments resolve.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionRequest {
    Run(SessionRunPlan),
    Restore,
}

/// The resolved plan for one session run.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRunPlan {
    pub settings: crate::session::settings::SettingFlags,
    pub jobs: Vec<crate::session::jobs::JobSpec>,
    pub stop_on_failure: bool,
    pub dry_run: bool,
    pub out: Option<PathBuf>,
    pub sample_seconds: u64,
}

impl SessionArgs {
    /// Resolves typed arguments into a session or the restore command.
    ///
    /// Fails with a message for a command line clap accepts but a session cannot use.
    pub fn resolve(self) -> Result<SessionRequest, String> {
        if self.action == Some(SessionAction::Restore) {
            let extra = self.spotlight_off
                || self.time_machine_off
                || self.wifi_off
                || self.jobs.is_some()
                || self.out.is_some()
                || self.sample_seconds.is_some()
                || self.stop_on_failure
                || self.dry_run
                || !self.command.is_empty();
            if extra {
                return Err("restore takes no other arguments".to_owned());
            }
            return Ok(SessionRequest::Restore);
        }

        if self.jobs.is_some() && !self.command.is_empty() {
            return Err("--jobs and -- cannot be used together".to_owned());
        }
        let jobs = match &self.jobs {
            Some(path) => crate::session::jobs::read_jobs_file(path)?,
            None if !self.command.is_empty() => {
                vec![crate::session::jobs::command_job(&self.command)?]
            }
            None => {
                return Err("pass --jobs <jobs.json> or -- <command...>".to_owned());
            }
        };

        Ok(SessionRequest::Run(SessionRunPlan {
            settings: crate::session::settings::SettingFlags {
                spotlight: self.spotlight_off,
                time_machine: self.time_machine_off,
                wifi: self.wifi_off,
            },
            jobs,
            stop_on_failure: self.stop_on_failure,
            dry_run: self.dry_run,
            out: self.out,
            sample_seconds: self
                .sample_seconds
                .unwrap_or(crate::session::DEFAULT_SAMPLE_SECONDS),
        }))
    }
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

    /// Optional samples written by benchdown session, for the contamination check
    #[arg(value_name = "SESSION_SAMPLES")]
    pub session_samples: Option<PathBuf>,

    /// Print the analysis as JSON instead of a table
    #[arg(long)]
    pub json: bool,
}

impl AnalyzeArgs {
    /// Reads the logs, analyzes them and prints the result.
    pub fn report(self) -> Result<(), String> {
        let power = read(&self.power)?;
        let block_log = read(&self.blocks)?;
        let samples = samples::parse_power_log(&power);
        let blocks = blocks::parse_blocks(&block_log)?;
        let contaminated = match &self.session_samples {
            Some(path) => {
                let session = crate::session::sampler::read_samples_file(path)?;
                crate::gate::find_contaminated_blocks(&session, &blocks)
            }
            None => Vec::new(),
        };
        let analysis = analysis::analyze(&samples, &blocks, &contaminated)?;

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

    /// Shell that runs each command; `none` splits it into words with quotes and
    /// backslashes, and runs it directly without expanding or interpreting anything
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

    /// Directory the measured commands run in
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Add KEY=VALUE to the measured commands' environment; repeatable
    #[arg(long, value_name = "KEY=VALUE", value_parser = parse_env_var)]
    pub env: Vec<(String, String)>,

    /// Start the measured commands with an empty environment
    #[arg(long)]
    pub env_clear: bool,

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
    /// Split the command into shell words: quotes group and protect characters
    /// and a backslash escapes the next one, but nothing is expanded and no
    /// operator is interpreted.
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
    /// Directory the measured commands run in.
    pub cwd: Option<PathBuf>,
    /// Variables added to the measured commands' environment, one per key.
    pub env: Vec<(String, String)>,
    /// Start the measured commands with an empty environment, then add `env`.
    pub env_clear: bool,
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
            .map(|(index, command)| {
                let name = args
                    .command_names
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| command.clone());
                let prepare = match args.prepare.len() {
                    0 => None,
                    1 => args.prepare.first().cloned(),
                    _ => args.prepare.get(index).cloned(),
                };
                // Resolve every string now, so a command the splitter cannot read
                // is a usage error rather than a failure after preflight.
                command_argv(&shell, &command).map_err(|error| format!("{name}: {error}"))?;
                if let Some(prepare) = &prepare {
                    command_argv(&shell, prepare)
                        .map_err(|error| format!("{name}: prepare {error}"))?;
                }
                Ok(Workload {
                    name,
                    command,
                    prepare,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        let mut env: Vec<(String, String)> = Vec::new();
        for (key, value) in args.env {
            match env.iter_mut().find(|(existing, _)| *existing == key) {
                Some(entry) => entry.1 = value,
                None => env.push((key, value)),
            }
        }

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
            cwd: args.cwd,
            env,
            env_clear: args.env_clear,
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

/// Splits one `--env KEY=VALUE` argument into its name and value.
fn parse_env_var(text: &str) -> Result<(String, String), String> {
    let Some((key, value)) = text.split_once('=') else {
        return Err(format!(
            "{text} is not KEY=VALUE: pass a variable name and a value joined by ="
        ));
    };
    if key.is_empty() {
        return Err(format!(
            "{text} has no variable name: pass KEY=VALUE with a name before the ="
        ));
    }
    Ok((key.to_owned(), value.to_owned()))
}

/// The argv that runs one command under a shell rule.
///
/// A shell gets the command string unchanged as its `-c` argument. Without a
/// shell the string is split into words: quotes group and protect characters and
/// a backslash escapes the next one, but nothing is expanded and no operator is
/// interpreted.
pub fn command_argv(shell: &Shell, command: &str) -> Result<Vec<String>, String> {
    match shell {
        Shell::Program(program) => Ok(vec![program.clone(), "-c".to_owned(), command.to_owned()]),
        Shell::None => {
            // shell-words keeps a trailing backslash as a literal character, but
            // an escape with nothing left to escape is a typo the user should see.
            if command
                .chars()
                .rev()
                .take_while(|character| *character == '\\')
                .count()
                % 2
                == 1
            {
                return Err(format!(
                    "cannot split {command:?} into an argv: trailing backslash"
                ));
            }
            let argv = shell_words::split(command)
                .map_err(|error| format!("cannot split {command:?} into an argv: {error}"))?;
            if argv.is_empty() {
                return Err(format!("{command:?} has no command to run"));
            }
            Ok(argv)
        }
    }
}

/// Seconds as `45s`, `7m 30s` or `1h 2m 3s`.
pub fn format_seconds(total: u64) -> String {
    let hours = total / 3_600;
    let minutes = total % 3_600 / 60;
    let seconds = total % 60;
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m {seconds}s"),
        _ => format!("{hours}h {minutes}m {seconds}s"),
    }
}

/// Prints a refused command line and exits with clap's usage-error status.
pub fn exit_with(message: &str) -> ! {
    exit_subcommand_with("run", message)
}

/// Prints a refused subcommand line and exits with clap's usage-error status.
pub fn exit_subcommand_with(name: &str, message: &str) -> ! {
    let mut command = Args::command();
    let mut sub = command
        .find_subcommand_mut(name)
        .unwrap_or_else(|| panic!("the {name} subcommand is declared"))
        .clone()
        .bin_name(format!("benchdown {name}"));
    sub.error(ErrorKind::InvalidValue, message.to_owned())
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
            ArgsCommand::Analyze(_) | ArgsCommand::Session(_) => {
                unreachable!("the helper always builds a run command")
            }
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
        assert_eq!(plan.cwd, None);
        assert!(plan.env.is_empty());
        assert!(!plan.env_clear);
        assert_eq!(plan.workloads.len(), 1);
        assert_eq!(plan.workloads[0].name, "echo hi");
        assert_eq!(plan.workloads[0].prepare, None);
    }

    #[test]
    fn cwd_and_environment_reach_the_plan() {
        let plan = plan(&[
            "--cwd",
            "/tmp/work",
            "--env",
            "A=1",
            "--env",
            "EMPTY=",
            "--env",
            "PAIR=a=b",
            "--env-clear",
            "true",
        ])
        .expect("the directory and environment parse");

        assert_eq!(plan.cwd, Some(PathBuf::from("/tmp/work")));
        assert_eq!(
            plan.env,
            [
                ("A".to_owned(), "1".to_owned()),
                ("EMPTY".to_owned(), String::new()),
                ("PAIR".to_owned(), "a=b".to_owned()),
            ]
        );
        assert!(plan.env_clear);
    }

    #[test]
    fn the_last_env_value_for_a_key_wins() {
        let plan = plan(&["--env", "A=1", "--env", "B=2", "--env", "A=3", "true"])
            .expect("a repeated key parses");
        assert_eq!(
            plan.env,
            [
                ("A".to_owned(), "3".to_owned()),
                ("B".to_owned(), "2".to_owned())
            ]
        );
    }

    #[test]
    fn an_env_without_an_equals_sign_is_refused() {
        let error = plan(&["--env", "NOVALUE", "true"]).expect_err("there is no value");
        assert!(error.contains("KEY=VALUE"), "{error}");
        assert!(error.contains("NOVALUE"), "{error}");
    }

    #[test]
    fn an_env_with_an_empty_key_is_refused() {
        let error = plan(&["--env", "=VALUE", "true"]).expect_err("there is no name");
        assert!(error.contains("KEY=VALUE"), "{error}");
        assert!(error.contains("=VALUE"), "{error}");
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
            "--cwd",
            "elsewhere",
            "--env",
            "X=1",
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
        assert_eq!(plan.cwd, Some(std::path::PathBuf::from("elsewhere")));
        assert_eq!(plan.env, [("X".to_owned(), "1".to_owned())]);
        assert!(!plan.env_clear);
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
    fn shell_none_still_splits_on_whitespace() {
        assert_eq!(
            command_argv(&Shell::None, "echo  hi").expect("the words split"),
            ["echo", "hi"]
        );
    }

    #[test]
    fn shell_none_keeps_quoted_arguments_together() {
        assert_eq!(
            command_argv(&Shell::None, "echo 'a b' c").expect("the words split"),
            ["echo", "a b", "c"]
        );
        assert_eq!(
            command_argv(&Shell::None, "echo \"a b\"").expect("the words split"),
            ["echo", "a b"]
        );
        assert_eq!(
            command_argv(&Shell::None, "echo a\\ b").expect("the words split"),
            ["echo", "a b"]
        );
    }

    #[test]
    fn shell_none_refuses_a_string_it_cannot_split() {
        for command in ["echo 'unterminated", "echo trailing\\"] {
            let error =
                command_argv(&Shell::None, command).expect_err("this string cannot be split");
            assert!(error.contains(command), "{error}");
        }
    }

    #[test]
    fn a_command_that_cannot_split_names_the_workload() {
        let error = plan(&["-n", "quoted", "-N", "echo 'unterminated"])
            .expect_err("the command cannot be split");
        assert!(error.contains("quoted"), "{error}");
        assert!(error.contains("echo 'unterminated"), "{error}");
    }

    #[test]
    fn a_prepare_command_that_cannot_split_is_refused() {
        let error = plan(&["-N", "-p", "echo 'unterminated", "true"])
            .expect_err("the prepare cannot be split");
        assert!(error.contains("prepare"), "{error}");
    }

    #[test]
    fn an_empty_shell_none_command_is_refused() {
        let error = plan(&["-N", "   "]).expect_err("there is nothing to run");
        assert!(error.contains("no command to run"), "{error}");
    }

    #[test]
    fn a_shell_command_passes_the_string_unchanged() {
        let command = "echo 'a b' | wc -l && true";
        assert_eq!(
            command_argv(&Shell::Program("sh".to_owned()), command).expect("a shell command runs"),
            ["sh", "-c", command]
        );
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
    fn seconds_are_formatted_by_size() {
        assert_eq!(format_seconds(0), "0s");
        assert_eq!(format_seconds(45), "45s");
        assert_eq!(format_seconds(60), "1m 0s");
        assert_eq!(format_seconds(450), "7m 30s");
        assert_eq!(format_seconds(3_723), "1h 2m 3s");
    }

    #[test]
    fn rounds_are_capped_so_a_plan_stays_printable() {
        let error = plan(&["--rounds", "20000", "one"]).expect_err("20000 rounds is refused");
        assert!(error.contains("20000"), "{error}");
    }
}
