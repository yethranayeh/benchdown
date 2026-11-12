//! benchdown measures a command's processor energy.

mod analysis;
mod blocks;
mod cli;
mod random;
mod samples;
mod schedule;

use clap::Parser;

fn main() {
    let args = cli::Args::parse();
    match args.command {
        cli::ArgsCommand::Run(run) => match cli::Plan::new(run) {
            Ok(plan) => print!("{plan}"),
            Err(message) => cli::exit_with(&message),
        },
        cli::ArgsCommand::Analyze(analyze) => {
            if let Err(message) = analyze.report() {
                eprintln!("error: {message}");
                std::process::exit(1);
            }
        }
    }
}
