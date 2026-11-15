//! benchdown measures a command's processor energy.

mod analysis;
mod blocks;
mod cli;
mod clock;
mod export;
mod gate;
mod meter;
mod platform;
mod powermetrics;
mod preflight;
mod random;
mod run;
mod runner;
mod samples;
mod schedule;
mod sensor;
mod session;

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
        cli::ArgsCommand::Session(args) => {
            let request = match args.resolve() {
                Ok(request) => request,
                Err(message) => cli::exit_subcommand_with("session", &message),
            };
            let system = runner::System::new();
            let code = match request {
                cli::SessionRequest::Restore => session::restore(&system),
                cli::SessionRequest::Run(plan) => session::run(&plan, &system),
            };
            std::process::exit(code);
        }
    }
}
