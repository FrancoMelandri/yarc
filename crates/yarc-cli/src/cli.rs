//! Command-line definition.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "yarc", version, about = "Yet Another Runtime Container")]
pub struct Cli {
    /// Where images and state are stored [default: per-user data directory]
    #[arg(long, global = true, env = "YARC_ROOT", value_name = "DIR")]
    pub root: Option<PathBuf>,

    /// Project file used by `up`, `down` and `project ...`
    #[arg(long, global = true, env = "YARC_FILE", value_name = "FILE", default_value = "yarc.yaml")]
    pub file: PathBuf,

    /// Project name [default: the name of the directory holding the project file]
    #[arg(short = 'p', long, global = true, value_name = "NAME")]
    pub project: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download an image from a registry into the local store
    Pull(PullArgs),
    /// Import images from an OCI layout archive (or directory) into the local store
    Load(LoadArgs),
    /// Work with images in the local store
    #[command(subcommand)]
    Image(ImageCommand),
    /// Create a container from an image without starting it (Linux only)
    Create(CreateArgs),
    /// Create a container and run it (Linux only)
    Run(RunArgs),
    /// List running containers
    Ps(PsArgs),
    /// Create and start all services of the project (Linux only)
    Up(UpArgs),
    /// Stop and remove all containers of the project (Linux only)
    Down(DownArgs),
    /// Inspect and control the project's services
    #[command(subcommand)]
    Project(ProjectCommand),
    /// Internal: supervises one detached container
    #[command(name = "__shim", hide = true)]
    Shim(ShimArgs),
    /// Check that containers can run here (Windows: in the WSL distro)
    Doctor,
    /// Internal: serves the CLI protocol on stdin/stdout for a client on another OS
    #[command(name = "agent", hide = true)]
    Agent(AgentArgs),
    /// Internal: owns a project's network namespace and publishes its ports
    #[command(name = "__holder", hide = true)]
    Holder(HolderArgs),
    /// Work with containers (the same commands also work without `container`)
    #[command(subcommand)]
    Container(ContainerCommand),
    #[command(flatten)]
    Direct(ContainerCommand),
}

#[derive(Debug, Args)]
pub struct UpArgs {
    /// Return once everything is started instead of streaming logs
    #[arg(short = 'd', long)]
    pub detach: bool,
}

#[derive(Debug, Args)]
pub struct DownArgs {
    /// Seconds to wait after SIGTERM before sending SIGKILL
    #[arg(short = 't', long, default_value_t = 10, value_name = "SECONDS")]
    pub time: u64,

    /// Also delete the project's named volumes (their data is lost)
    #[arg(short = 'v', long)]
    pub volumes: bool,
}

#[derive(Debug, Args)]
pub struct StopProjectArgs {
    /// Seconds to wait after SIGTERM before sending SIGKILL
    #[arg(short = 't', long, default_value_t = 10, value_name = "SECONDS")]
    pub time: u64,
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// List the project's containers
    Ps,
    /// Show the project's logs, prefixed with the service name
    Logs(ProjectLogsArgs),
    /// Stop the project's containers (without removing them)
    Stop(StopProjectArgs),
    /// Print the resolved project: name, start order, container names
    Config,
}

#[derive(Debug, Args)]
pub struct ProjectLogsArgs {
    /// Only these services [default: all]
    pub services: Vec<String>,

    /// Only the last N lines of each service
    #[arg(long, value_name = "N")]
    pub tail: Option<usize>,

    /// Prefix each line with its timestamp
    #[arg(short = 't', long)]
    pub timestamps: bool,

    /// Keep streaming until all containers have exited
    #[arg(short = 'f', long)]
    pub follow: bool,
}

#[derive(Debug, Args)]
pub struct PsArgs {
    /// Show all containers, not just running ones
    #[arg(short = 'a', long)]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct AgentArgs {
    /// Speak the protocol on stdin and stdout (the only transport so far)
    #[arg(long, required = true)]
    pub stdio: bool,
}

#[derive(Debug, Args)]
pub struct HolderArgs {
    pub project: String,
    /// `HOST:CONTAINER` TCP port to publish on 127.0.0.1 (repeatable)
    #[arg(long = "publish", value_name = "HOST:CONTAINER")]
    pub publish: Vec<String>,
    /// The `pasta` binary that gives the network outbound access (none: no outbound)
    #[arg(long, value_name = "PATH")]
    pub pasta: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ShimArgs {
    pub id: String,
    #[arg(long)]
    pub debug: bool,
    /// Remove the container when it exits
    #[arg(long)]
    pub rm: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Remove the container when it exits
    #[arg(long)]
    pub rm: bool,

    /// Run in the background and print the container id
    #[arg(short = 'd', long)]
    pub detach: bool,

    /// Print the setup plan and skipped optional steps
    #[arg(long)]
    pub debug: bool,

    #[command(flatten)]
    pub create: CreateArgs,
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
    /// Run a command inside a running container
    Exec(ExecArgs),
    /// Stop running containers: SIGTERM, then SIGKILL after a timeout
    Stop(StopArgs),
    /// Send a signal to running containers (default SIGKILL)
    Kill(KillArgs),
    /// Block until containers exit and print their exit codes
    Wait(WaitArgs),
    /// Show what a container printed
    Logs(LogsArgs),
    /// Remove containers and their root filesystems
    Rm(RmArgs),
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    /// Set an environment variable for the command (repeatable)
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Working directory inside the container [default: the container's]
    #[arg(short = 'w', long, value_name = "DIR")]
    pub workdir: Option<String>,

    /// Container id or name
    pub id: String,

    /// Command and arguments
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,
}

#[derive(Debug, Args)]
pub struct StopArgs {
    /// Seconds to wait after SIGTERM before sending SIGKILL
    #[arg(short = 't', long, default_value_t = 10, value_name = "SECONDS")]
    pub time: u64,

    /// Container ids or names
    #[arg(required = true)]
    pub ids: Vec<String>,
}

#[derive(Debug, Args)]
pub struct KillArgs {
    /// Signal name (TERM, SIGUSR1, ...) or number
    #[arg(short = 's', long, default_value = "KILL", value_name = "SIGNAL")]
    pub signal: String,

    /// Container ids or names
    #[arg(required = true)]
    pub ids: Vec<String>,
}

#[derive(Debug, Args)]
pub struct WaitArgs {
    /// Container ids or names
    #[arg(required = true)]
    pub ids: Vec<String>,
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// Container id or name
    pub id: String,

    /// Run in the background and print the container id
    #[arg(short = 'd', long)]
    pub detach: bool,

    /// Print the setup plan and skipped optional steps
    #[arg(long)]
    pub debug: bool,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Container id or name
    pub id: String,

    /// Only the last N lines
    #[arg(long, value_name = "N")]
    pub tail: Option<usize>,

    /// Prefix each line with its timestamp
    #[arg(short = 't', long)]
    pub timestamps: bool,

    /// Keep streaming new output until the container exits
    #[arg(short = 'f', long)]
    pub follow: bool,
}

#[derive(Debug, Args)]
pub struct RmArgs {
    /// Kill running containers first
    #[arg(short = 'f', long)]
    pub force: bool,

    /// Container ids or names
    #[arg(required = true)]
    pub ids: Vec<String>,
}

#[derive(Debug, Args)]
pub struct LoadArgs {
    /// Tar archive of an OCI image layout (e.g. images.tar), or the layout directory
    pub file: PathBuf,
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
    /// List the images in the local store
    #[command(visible_alias = "list")]
    Ls,
    /// Remove images from the local store, freeing layers no other image uses
    Rm(ImageRmArgs),
    /// Extract a pulled image into a directory (Linux only)
    Unpack(UnpackArgs),
}

#[derive(Debug, Args)]
pub struct ImageRmArgs {
    /// Image references, as given to `pull` or shown by `image ls`
    #[arg(required = true)]
    pub images: Vec<String>,
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
    fn load_takes_a_path() {
        let cli = parse(&["load", "images.tar"]).unwrap();
        let Command::Load(a) = cli.command else { panic!() };
        assert_eq!(a.file, PathBuf::from("images.tar"));
        assert!(parse(&["load"]).is_err());
    }

    #[test]
    fn image_ls_and_rm() {
        assert!(matches!(parse(&["image", "ls"]).unwrap().command, Command::Image(ImageCommand::Ls)));
        assert!(matches!(parse(&["image", "list"]).unwrap().command, Command::Image(ImageCommand::Ls)));
        let Command::Image(ImageCommand::Rm(a)) = parse(&["image", "rm", "a:1", "b"]).unwrap().command else { panic!() };
        assert_eq!(a.images, ["a:1", "b"]);
        assert!(parse(&["image", "rm"]).is_err());
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
    fn run_takes_create_options_plus_rm_and_debug() {
        let cli = parse(&["run", "--rm", "--debug", "--name", "web", "-e", "A=1", "alpine", "sh", "-c", "echo hi", "--rm"]).unwrap();
        let Command::Run(a) = cli.command else { panic!() };
        assert!(a.rm && a.debug);
        assert_eq!((a.create.name.as_deref(), a.create.image.as_str()), (Some("web"), "alpine"));
        assert_eq!(a.create.env, ["A=1"]);
        // flags after the image belong to the container's command
        assert_eq!(a.create.cmd, ["sh", "-c", "echo hi", "--rm"]);
    }

    #[test]
    fn run_defaults_keep_the_container() {
        let Command::Run(a) = parse(&["run", "alpine"]).unwrap().command else { panic!() };
        assert!(!a.rm && !a.debug);
        assert!(parse(&["run"]).is_err());
    }

    #[test]
    fn detach_flags_and_ps() {
        let Command::Run(a) = parse(&["run", "-d", "--rm", "alpine", "echo", "-d"]).unwrap().command else { panic!() };
        assert!(a.detach && a.rm);
        assert_eq!(a.create.cmd, ["echo", "-d"], "flags after the command belong to the container");
        // a command that itself starts with a dash needs `--`
        let Command::Run(a) = parse(&["run", "alpine", "--", "-d"]).unwrap().command else { panic!() };
        assert!(!a.detach);
        assert_eq!(a.create.cmd, ["-d"]);
        let Command::Run(a) = parse(&["run", "alpine"]).unwrap().command else { panic!() };
        assert!(!a.detach);
        let Command::Container(ContainerCommand::Start(a)) = parse(&["container", "start", "-d", "web"]).unwrap().command else { panic!() };
        assert!(a.detach);
        let Command::Ps(a) = parse(&["ps"]).unwrap().command else { panic!() };
        assert!(!a.all);
        let Command::Ps(a) = parse(&["ps", "-a"]).unwrap().command else { panic!() };
        assert!(a.all);
    }

    #[test]
    fn the_holder_command_exists_but_is_hidden() {
        let Command::Holder(a) = parse(&["__holder", "demo", "--publish", "8080:80", "--publish", "9:9"]).unwrap().command else { panic!() };
        assert_eq!((a.project.as_str(), a.publish.as_slice()), ("demo", &["8080:80".to_string(), "9:9".to_string()][..]));
        use clap::CommandFactory;
        assert!(!Cli::command().render_help().to_string().contains("__holder"));
    }

    #[test]
    fn the_shim_command_exists_but_is_hidden() {
        let Command::Shim(a) = parse(&["__shim", "web", "--rm", "--debug"]).unwrap().command else { panic!() };
        assert_eq!((a.id.as_str(), a.rm, a.debug), ("web", true, true));
        use clap::CommandFactory;
        let help = Cli::command().render_help().to_string();
        assert!(!help.contains("__shim"), "{help}");
    }

    #[test]
    fn container_start() {
        let Command::Container(ContainerCommand::Start(a)) = parse(&["container", "start", "web"]).unwrap().command else { panic!() };
        assert_eq!((a.id.as_str(), a.debug, a.detach), ("web", false, false));
        let Command::Container(ContainerCommand::Start(a)) = parse(&["container", "start", "--debug", "web"]).unwrap().command else { panic!() };
        assert!(a.debug);
        assert!(parse(&["container", "start"]).is_err());
    }

    #[test]
    fn container_logs() {
        let Command::Container(ContainerCommand::Logs(a)) = parse(&["container", "logs", "web"]).unwrap().command else { panic!() };
        assert_eq!((a.id.as_str(), a.tail, a.timestamps), ("web", None, false));
        let Command::Container(ContainerCommand::Logs(a)) = parse(&["container", "logs", "--tail", "5", "-t", "web"]).unwrap().command else { panic!() };
        assert_eq!((a.tail, a.timestamps), (Some(5), true));
        assert!(parse(&["container", "logs", "--tail", "x", "web"]).is_err());
    }

    #[test]
    fn project_commands_and_global_project_options() {
        let cli = parse(&["up"]).unwrap();
        assert_eq!((cli.file.as_path(), cli.project.as_deref()), (std::path::Path::new("yarc.yaml"), None));
        let Command::Up(a) = cli.command else { panic!() };
        assert!(!a.detach);
        // global options work before and after the subcommand
        let cli = parse(&["--file", "x.yaml", "-p", "demo", "up", "-d"]).unwrap();
        assert_eq!((cli.file.as_path(), cli.project.as_deref()), (std::path::Path::new("x.yaml"), Some("demo")));
        let cli = parse(&["down", "-t", "2", "--file", "y.yaml", "--project", "p2"]).unwrap();
        assert_eq!((cli.file.as_path(), cli.project.as_deref()), (std::path::Path::new("y.yaml"), Some("p2")));
        let Command::Down(a) = cli.command else { panic!() };
        assert_eq!((a.time, a.volumes), (2, false));
        let Command::Down(a) = parse(&["down", "-v"]).unwrap().command else { panic!() };
        assert!(a.volumes);
        assert!(parse(&["project", "stop", "-v"]).is_err(), "stop never deletes volumes");
        assert!(matches!(parse(&["project", "ps"]).unwrap().command, Command::Project(ProjectCommand::Ps)));
        assert!(matches!(parse(&["project", "config"]).unwrap().command, Command::Project(ProjectCommand::Config)));
        let Command::Project(ProjectCommand::Stop(a)) = parse(&["project", "stop"]).unwrap().command else { panic!() };
        assert_eq!(a.time, 10);
        let Command::Project(ProjectCommand::Logs(a)) = parse(&["project", "logs", "-f", "-t", "--tail", "5", "web", "db"]).unwrap().command else { panic!() };
        assert!(a.follow && a.timestamps);
        assert_eq!((a.tail, a.services.as_slice()), (Some(5), &["web".to_string(), "db".to_string()][..]));
    }

    #[test]
    fn exec_takes_options_before_the_container_and_a_command_after_it() {
        let Command::Container(ContainerCommand::Exec(a)) =
            parse(&["container", "exec", "-e", "A=1", "-w", "/tmp", "web", "sh", "-c", "echo $A", "--flag"]).unwrap().command
        else {
            panic!()
        };
        assert_eq!((a.env.as_slice(), a.workdir.as_deref(), a.id.as_str()), (&["A=1".to_string()][..], Some("/tmp"), "web"));
        assert_eq!(a.cmd, ["sh", "-c", "echo $A", "--flag"]);
        assert!(matches!(parse(&["exec", "web", "ls"]).unwrap().command, Command::Direct(ContainerCommand::Exec(_))));
        assert!(parse(&["exec", "web"]).is_err(), "a command is required");
        assert!(parse(&["exec"]).is_err());
    }

    #[test]
    fn control_commands() {
        let Command::Container(ContainerCommand::Stop(a)) = parse(&["container", "stop", "a", "b"]).unwrap().command else { panic!() };
        assert_eq!((a.time, a.ids.as_slice()), (10, &["a".to_string(), "b".to_string()][..]));
        let Command::Container(ContainerCommand::Stop(a)) = parse(&["container", "stop", "-t", "3", "a"]).unwrap().command else { panic!() };
        assert_eq!(a.time, 3);
        let Command::Container(ContainerCommand::Kill(a)) = parse(&["container", "kill", "a"]).unwrap().command else { panic!() };
        assert_eq!(a.signal, "KILL");
        let Command::Container(ContainerCommand::Kill(a)) = parse(&["container", "kill", "-s", "USR1", "a"]).unwrap().command else { panic!() };
        assert_eq!(a.signal, "USR1");
        let Command::Container(ContainerCommand::Wait(a)) = parse(&["container", "wait", "a"]).unwrap().command else { panic!() };
        assert_eq!(a.ids, ["a"]);
        let Command::Container(ContainerCommand::Rm(a)) = parse(&["container", "rm", "-f", "a"]).unwrap().command else { panic!() };
        assert!(a.force);
        let Command::Container(ContainerCommand::Logs(a)) = parse(&["container", "logs", "-f", "a"]).unwrap().command else { panic!() };
        assert!(a.follow);
        for bad in [&["container", "stop"][..], &["container", "kill"], &["container", "wait"], &["container", "stop", "-t", "x", "a"]] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn container_commands_also_work_without_the_container_prefix() {
        assert!(matches!(parse(&["stop", "a"]).unwrap().command, Command::Direct(ContainerCommand::Stop(_))));
        assert!(matches!(parse(&["logs", "-f", "a"]).unwrap().command, Command::Direct(ContainerCommand::Logs(_))));
        assert!(matches!(parse(&["rm", "-f", "a"]).unwrap().command, Command::Direct(ContainerCommand::Rm(_))));
        assert!(matches!(parse(&["start", "-d", "a"]).unwrap().command, Command::Direct(ContainerCommand::Start(_))));
        assert!(matches!(parse(&["ls"]).unwrap().command, Command::Direct(ContainerCommand::Ls)));
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
