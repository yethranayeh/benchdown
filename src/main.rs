//! benchdown measures a command's processor energy.

mod analysis;
mod blocks;
mod cli;
mod clock;
mod gate;
mod powermetrics;
mod preflight;
mod random;
mod run;
mod runner;
mod samples;
mod schedule;

use clap::Parser;

fn main() {
    let args = cli::Args::parse();
    match args.command {
        cli::ArgsCommand::Run(run_args) => {
            let plan = match cli::Plan::new(run_args) {
                Ok(plan) => plan,
                Err(message) => cli::exit_with(&message),
            };
            if let Err(message) = run::run(&plan, &runner::System::new()) {
                eprintln!("error: {message}");
                std::process::exit(1);
            }
        }
        cli::ArgsCommand::Analyze(analyze) => {
            if let Err(message) = analyze.report() {
                eprintln!("error: {message}");
                std::process::exit(1);
            }
        }
    }
}
