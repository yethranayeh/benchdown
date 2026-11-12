//! benchdown measures a command's processor energy.

mod cli;

use clap::Parser;

fn main() {
    let args = cli::Args::parse();
    match args.command {
        cli::ArgsCommand::Run(run) => match cli::Plan::new(run) {
            Ok(plan) => print!("{plan}"),
            Err(message) => cli::exit_with(&message),
        },
    }
}
