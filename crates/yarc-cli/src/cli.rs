//! Command-line definition.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "yarc", version, about = "Yet Another Runtime Container")]
pub struct Cli {
    /// Where images and state are stored [default: per-user data directory]
    #[arg(long, global = true, env = "YARC_ROOT", value_name = "DIR")]
    pub root: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download an image from a registry into the local store
    Pull(PullArgs),
    /// Work with images in the local store
    #[command(subcommand)]
    Image(ImageCommand),
    /// Create a container from an image without starting it (Linux only)
    Create(CreateArgs),
    /// Work with containers
    #[command(subcommand)]
    Container(ContainerCommand),
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// Container name [default: random id]
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Set an environment variable (repeatable)
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Override the image's entrypoint ("" clears it)
    #[arg(long, value_name = "CMD")]
    pub entrypoint: Option<String>,

    /// Working directory inside the container
    #[arg(short = 'w', long, value_name = "DIR")]
    pub workdir: Option<String>,

    /// User to run as (name or uid[:gid])
    #[arg(short = 'u', long, value_name = "USER")]
    pub user: Option<String>,

    /// Network mode: host (share the host's) or none (loopback only)
    #[arg(long, value_name = "MODE", default_value = "host")]
    pub network: yarc_runtime::Network,

    /// Image reference; pulled automatically when missing
    pub image: String,

    /// Command and arguments, replacing the image's Cmd
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum ContainerCommand {
    /// List containers
    Ls,
    /// Start a created container in the foreground (Linux only)
    Start(StartArgs),
    /// Remove containers and their root filesystems
    Rm(RmArgs),
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// Container id or name
    pub id: String,

    /// Print the setup plan and skipped optional steps
    #[arg(long)]
    pub debug: bool,
}

#[derive(Debug, Args)]
pub struct RmArgs {
    /// Container ids or names
    #[arg(required = true)]
    pub ids: Vec<String>,
}

#[derive(Debug, Args)]
pub struct PullArgs {
    /// Image reference, e.g. alpine, postgres:16, ghcr.io/org/app:1.2
    pub image: String,

    /// Parallel layer downloads
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=64))]
    pub jobs: u16,

    /// Platform to pull, as OS/ARCH [default: linux/<this machine's arch>]
    #[arg(long, value_name = "OS/ARCH")]
    pub platform: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ImageCommand {
    /// Extract a pulled image into a directory (Linux only)
    Unpack(UnpackArgs),
}

#[derive(Debug, Args)]
pub struct UnpackArgs {
    /// Image reference that was pulled before
    pub image: String,
    /// Destination directory (must not exist or be empty)
    pub dir: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("yarc").chain(args.iter().copied()))
    }

    #[test]
    fn pull_defaults() {
        let cli = parse(&["pull", "alpine"]).unwrap();
        let Command::Pull(a) = cli.command else { panic!() };
        assert_eq!(a.image, "alpine");
        assert_eq!(a.jobs, 4);
        assert_eq!(a.platform, None);
    }

    #[test]
    fn pull_options_and_global_root() {
        let cli = parse(&["pull", "--jobs", "8", "--platform", "linux/arm64", "--root", "/tmp/y", "nginx:1"]).unwrap();
        assert_eq!(cli.root, Some(PathBuf::from("/tmp/y")));
        let Command::Pull(a) = cli.command else { panic!() };
        assert_eq!((a.jobs, a.platform.as_deref()), (8, Some("linux/arm64")));
    }

    #[test]
    fn image_unpack() {
        let cli = parse(&["image", "unpack", "alpine", "./rootfs"]).unwrap();
        let Command::Image(ImageCommand::Unpack(a)) = cli.command else { panic!() };
        assert_eq!((a.image.as_str(), a.dir), ("alpine", PathBuf::from("./rootfs")));
    }

    #[test]
    fn create_defaults() {
        let cli = parse(&["create", "alpine"]).unwrap();
        let Command::Create(a) = cli.command else { panic!() };
        assert_eq!(a.image, "alpine");
        assert!(a.cmd.is_empty() && a.env.is_empty());
        assert_eq!(a.network, yarc_runtime::Network::Host);
        assert_eq!((a.name, a.entrypoint, a.workdir, a.user), (None, None, None, None));
    }

    #[test]
    fn create_options_before_the_image_and_command_after() {
        let cli = parse(&[
            "create", "--name", "web", "-e", "A=1", "-e", "B=2", "--entrypoint", "/bin/sh", "-w", "/app",
            "--network", "none", "alpine", "-c", "echo $A", "--not-an-option",
        ])
        .unwrap();
        let Command::Create(a) = cli.command else { panic!() };
        assert_eq!(a.name.as_deref(), Some("web"));
        assert_eq!(a.env, ["A=1", "B=2"]);
        assert_eq!((a.entrypoint.as_deref(), a.workdir.as_deref()), (Some("/bin/sh"), Some("/app")));
        assert_eq!(a.network, yarc_runtime::Network::None);
        assert_eq!(a.image, "alpine");
        // everything after the image belongs to the container's command
        assert_eq!(a.cmd, ["-c", "echo $A", "--not-an-option"]);
    }

    #[test]
    fn container_subcommands() {
        assert!(matches!(parse(&["container", "ls"]).unwrap().command, Command::Container(ContainerCommand::Ls)));
        let Command::Container(ContainerCommand::Rm(a)) = parse(&["container", "rm", "a", "b"]).unwrap().command else { panic!() };
        assert_eq!(a.ids, ["a", "b"]);
        assert!(parse(&["container", "rm"]).is_err());
    }

    #[test]
    fn container_start() {
        let Command::Container(ContainerCommand::Start(a)) = parse(&["container", "start", "web"]).unwrap().command else { panic!() };
        assert_eq!((a.id.as_str(), a.debug), ("web", false));
        let Command::Container(ContainerCommand::Start(a)) = parse(&["container", "start", "--debug", "web"]).unwrap().command else { panic!() };
        assert!(a.debug);
        assert!(parse(&["container", "start"]).is_err());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse(&["create"]).is_err());
        assert!(parse(&["create", "--network", "bridge", "alpine"]).is_err());
        assert!(parse(&[]).is_err());
        assert!(parse(&["pull"]).is_err());
        assert!(parse(&["pull", "x", "--jobs", "0"]).is_err());
        assert!(parse(&["image", "unpack", "alpine"]).is_err());
        assert!(parse(&["frobnicate"]).is_err());
    }
}
