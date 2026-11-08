mod cli;
mod commands;
mod root;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command, ContainerCommand, ImageCommand};

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the process exit code (non-zero only when a container's own status
/// is passed through).
fn run() -> Result<u8> {
    let cli = Cli::parse();
    let root = match cli.root {
        Some(r) => r,
        None => root::default_root()?,
    };
    match cli.command {
        Command::Container(ContainerCommand::Start(args)) => commands::container_start(&root, args),
        Command::Pull(args) => commands::pull(&root, args).map(|()| 0),
        Command::Image(ImageCommand::Unpack(args)) => commands::unpack(&root, args).map(|()| 0),
        Command::Create(args) => commands::create(&root, args).map(|()| 0),
        Command::Container(ContainerCommand::Ls) => commands::container_ls(&root).map(|()| 0),
        Command::Container(ContainerCommand::Rm(args)) => commands::container_rm(&root, args).map(|()| 0),
    }
}
