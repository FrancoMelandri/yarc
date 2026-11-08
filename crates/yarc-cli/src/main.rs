mod cli;
mod commands;
mod root;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command, ContainerCommand, ImageCommand};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = match cli.root {
        Some(r) => r,
        None => root::default_root()?,
    };
    match cli.command {
        Command::Pull(args) => commands::pull(&root, args),
        Command::Image(ImageCommand::Unpack(args)) => commands::unpack(&root, args),
        Command::Create(args) => commands::create(&root, args),
        Command::Container(ContainerCommand::Ls) => commands::container_ls(&root),
        Command::Container(ContainerCommand::Rm(args)) => commands::container_rm(&root, args),
    }
}
