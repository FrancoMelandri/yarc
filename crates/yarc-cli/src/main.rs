mod cli;
mod commands;
mod control;
// Running containers and projects is Linux code: on other systems these
// commands are forwarded to the agent in WSL (`remote`), so nothing calls the
// helpers below there. They stay compiled so their tests run everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]
mod detached;
#[cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]
mod project;
mod remote;
mod root;

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command, ContainerCommand, ImageCommand, ProjectCommand};

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("error: {err:#}");
            // Docker-style status for a container that could not be started
            // (127 command not found, 126 not executable, 125 other).
            match err.downcast_ref::<yarc_runtime::RuntimeError>() {
                Some(yarc_runtime::RuntimeError::Start { code, .. }) => ExitCode::from(*code as u8),
                _ => ExitCode::FAILURE,
            }
        }
    }
}

/// Returns the process exit code (non-zero only when a container's own status
/// is passed through).
fn run() -> Result<u8> {
    let cli = Cli::parse();
    if let Some(code) = remote_or_doctor(&cli)? {
        return Ok(code);
    }
    let file = cli.project_file();
    let root = match cli.root {
        Some(r) => r,
        None => root::default_root()?,
    };
    let opts = project::Opts { file, name: cli.project, profiles: cli.profile, env_files: cli.env_file };
    match cli.command {
        Command::Run(args) => commands::run(&root, args),
        Command::Pull(args) => commands::pull(&root, args).map(|()| 0),
        Command::Load(args) => commands::load(&root, args).map(|()| 0),
        Command::Image(ImageCommand::Ls) => commands::image_ls(&root).map(|()| 0),
        Command::Image(ImageCommand::Rm(args)) => commands::image_rm(&root, args).map(|()| 0),
        Command::Image(ImageCommand::Unpack(args)) => commands::unpack(&root, args).map(|()| 0),
        Command::Create(args) => commands::create(&root, args).map(|()| 0),
        Command::Ps(args) => commands::container_ls(&root, args.all).map(|()| 0),
        Command::Shim(args) => detached::shim(&root, args).map(|()| 0),
        Command::Holder(args) => detached::holder(&root, args).map(|()| 0),
        Command::Agent(_) => agent().map(|()| 0),
        Command::Doctor => Ok(0), // handled before the store is touched
        Command::Up(args) => project::up(&root, &opts, args.detach),
        Command::Down(args) => {
            project::down(&root, &opts, args.time.map(Duration::from_secs), args.volumes).map(|()| 0)
        }
        Command::Project(cmd) => match cmd {
            ProjectCommand::Ps => project::ps(&root, &opts).map(|()| 0),
            ProjectCommand::Logs(args) => project::logs(&root, &opts, args).map(|()| 0),
            ProjectCommand::Stop(args) => project::stop(&root, &opts, args.time.map(Duration::from_secs)).map(|()| 0),
            ProjectCommand::Config => project::config(&opts).map(|()| 0),
        },
        Command::Container(cmd) | Command::Direct(cmd) => container(&root, cmd),
    }
}

fn container(root: &Path, cmd: ContainerCommand) -> Result<u8> {
    match cmd {
        ContainerCommand::Ls => commands::container_ls(root, true).map(|()| 0),
        ContainerCommand::Start(args) => commands::container_start(root, args),
        ContainerCommand::Exec(args) => control::exec(root, args),
        ContainerCommand::Stop(args) => control::stop(root, args).map(|()| 0),
        ContainerCommand::Kill(args) => control::kill(root, args).map(|()| 0),
        ContainerCommand::Wait(args) => control::wait(root, args).map(|()| 0),
        ContainerCommand::Logs(args) => commands::container_logs(root, args).map(|()| 0),
        ContainerCommand::Rm(args) => control::rm(root, args).map(|()| 0),
    }
}

/// `yarc agent --stdio`: serves protocol requests, running this same binary for each.
#[cfg(target_os = "linux")]
fn agent() -> Result<()> {
    use anyhow::Context;

    let exe = std::env::current_exe().context("cannot locate the yarc executable")?;
    // The agent is the far end of the forwarding: the commands it runs must
    // run, not be forwarded again (`YARC_REMOTE` would make them loop forever).
    std::env::remove_var("YARC_REMOTE");
    yarc_agent::serve(std::io::stdin().lock(), std::io::stdout(), &exe)?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn agent() -> Result<()> {
    anyhow::bail!("the agent serves requests on Linux; run yarc inside WSL")
}

/// `doctor`, and every command that is forwarded to the agent in WSL instead of
/// running here. `None`: run it locally.
fn remote_or_doctor(cli: &Cli) -> Result<Option<u8>> {
    use remote::{doctor, forward, wsl::Wsl};

    if matches!(cli.command, Command::Doctor) {
        let checks = if remote::enabled() {
            let wsl = Wsl::from_env();
            doctor::checks(&wsl, &wsl.bin.clone(), true)
        } else {
            let exe = std::env::current_exe()?;
            doctor::checks(&doctor::Local, &exe.to_string_lossy(), false)
        };
        print!("{}", doctor::render(&checks));
        return Ok(Some(doctor::exit_code(&checks) as u8));
    }
    if !remote::enabled() {
        return Ok(None);
    }
    let wsl = Wsl::from_env();
    let paths = forward::Paths { distro: wsl.distro.as_deref(), absolute: remote::winpath::absolutize };
    match forward::plan(cli, &paths)? {
        Some(request) => Ok(Some((wsl.run(&request)? & 0xff) as u8)),
        None => Ok(None),
    }
}
