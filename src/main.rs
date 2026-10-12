mod cd_target;
mod cli;
mod commands;
mod completion;
mod config;
mod git;
mod hooks;
mod repo;
mod tui;

use clap::{CommandFactory, Parser};

use cli::{Cli, Command};
use commands::shell::COMPLETE_VAR;

fn main() {
    // Answers completion requests and exits; a no-op on a normal run. Must come
    // before anything is written to stdout.
    clap_complete::CompleteEnv::with_factory(Cli::command)
        .var(COMPLETE_VAR)
        .bin("gwx")
        .completer("gwx")
        .complete();

    if let Err(err) = run() {
        eprintln!("gwx: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Add(args) => commands::add::run(args),
        Command::List(args) => commands::list::run(args),
        Command::Cd(args) => commands::cd::run(args),
        Command::Remove(args) => commands::remove::run(args),
        Command::Clean(args) => commands::clean::run(args),
        Command::Init(args) => commands::init::run(args),
        Command::Alias(args) => commands::alias::run(args),
        Command::ShellInit(args) => commands::shell::shell_init(args),
        Command::Completion(args) => commands::shell::completion(args),
        Command::Man(args) => commands::man::run(args),
    }
}
