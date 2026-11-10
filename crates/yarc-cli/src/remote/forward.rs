//! What the parsed command line asks for, as a protocol request.

use std::path::Path;

use anyhow::{Context, Result};
use yarc_proto::{Command, CreateSpec, Globals};

use super::winpath;
use crate::cli::{self, Cli, ContainerCommand, ImageCommand, ProjectCommand};

/// A request ready to send to the agent.
#[derive(Debug, PartialEq, Eq)]
pub struct Forward {
    pub globals: Globals,
    pub command: Command,
}

/// Turns the paths of this machine into the paths the agent sees.
pub struct Paths<'a> {
    /// The WSL distro the agent runs in, when known.
    pub distro: Option<&'a str>,
    /// Makes a path absolute (identity in tests).
    pub absolute: fn(&Path) -> std::io::Result<String>,
}

impl Paths<'_> {
    pub fn linux(&self, path: &Path) -> Result<String> {
        let abs = (self.absolute)(path).with_context(|| format!("cannot resolve {}", path.display()))?;
        winpath::to_linux(&abs, self.distro).map_err(Into::into)
    }
}

/// The request for `cli`, or `None` for a command that is handled on this machine
/// (`project config`, which only reads the project file, and the internal commands).
pub fn plan(cli: &Cli, paths: &Paths) -> Result<Option<Forward>> {
    use cli::Command as C;
    let needs_project = matches!(cli.command, C::Up(_) | C::Down(_) | C::Project(_));
    let command = match &cli.command {
        C::Pull(a) => Command::Pull { image: a.image.clone(), jobs: Some(a.jobs), platform: a.platform.clone() },
        C::Load(a) => Command::Load { file: paths.linux(&a.file)? },
        C::Image(ImageCommand::Ls) => Command::ImageLs,
        C::Image(ImageCommand::Rm(a)) => Command::ImageRm { images: a.images.clone() },
        C::Image(ImageCommand::Unpack(a)) => Command::ImageUnpack { image: a.image.clone(), dir: paths.linux(&a.dir)? },
        C::Create(a) => Command::Create(create_spec(a)?),
        C::Run(a) => Command::Run { spec: create_spec(&a.create)?, rm: a.rm, detach: a.detach, debug: a.debug },
        C::Ps(a) => Command::Ps { all: a.all },
        C::Stats(a) => Command::Stats { ids: a.ids.clone() },
        C::Up(a) => Command::Up { detach: a.detach },
        C::Down(a) => Command::Down { time: a.time, volumes: a.volumes },
        C::Project(ProjectCommand::Ps) => Command::ProjectPs,
        C::Project(ProjectCommand::Logs(a)) => Command::ProjectLogs {
            services: a.services.clone(),
            tail: a.tail,
            timestamps: a.timestamps,
            follow: a.follow,
        },
        C::Project(ProjectCommand::Stop(a)) => Command::ProjectStop { time: a.time },
        C::Project(ProjectCommand::Config) => return Ok(None),
        C::Container(c) | C::Direct(c) => container_command(c),
        C::Doctor | C::Agent(_) | C::Shim(_) | C::Holder(_) => return Ok(None),
    };
    let globals = Globals {
        root: cli.root.as_deref().map(|r| paths.linux(r)).transpose()?,
        // discovery (compose.yaml, ...) must look at the Windows directory, not the translated path
        file: if needs_project { Some(paths.linux(&crate::project::resolve_file(&cli.project_file()))?) } else { None },
        project: cli.project.clone(),
        profiles: if needs_project { cli.profile.clone() } else { Vec::new() },
        env_files: if needs_project {
            cli.env_file.iter().map(|p| paths.linux(p)).collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        },
    };
    Ok(Some(Forward { globals, command }))
}

/// A bad combination of limits is reported here, before the agent is bothered.
fn create_spec(a: &cli::CreateArgs) -> Result<CreateSpec> {
    a.resources().map_err(anyhow::Error::msg)?;
    Ok(CreateSpec {
        image: a.image.clone(),
        name: a.name.clone(),
        env: a.env.clone(),
        entrypoint: a.entrypoint.clone(),
        workdir: a.workdir.clone(),
        user: a.user.clone(),
        network: Some(
            match a.network {
                yarc_runtime::Network::Host => "host",
                yarc_runtime::Network::None => "none",
                // not selectable on the command line
                yarc_runtime::Network::Project => "host",
            }
            .to_string(),
        ),
        restart: (!a.restart.is_no()).then(|| a.restart.to_string()),
        memory: a.memory.map(|n| n.to_string()),
        memory_swap: a.memory_swap.map(|n| n.to_string()),
        cpus: a.cpus.map(yarc_config::format_cpus),
        pids_limit: a.pids_limit.map(|n| n.to_string()),
        cmd: a.cmd.clone(),
    })
}

fn container_command(c: &ContainerCommand) -> Command {
    match c {
        ContainerCommand::Ls => Command::ContainerLs,
        ContainerCommand::Start(a) => Command::Start { id: a.id.clone(), detach: a.detach, debug: a.debug },
        ContainerCommand::Exec(a) => {
            Command::Exec { id: a.id.clone(), env: a.env.clone(), workdir: a.workdir.clone(), cmd: a.cmd.clone() }
        }
        ContainerCommand::Stop(a) => Command::Stop { ids: a.ids.clone(), time: Some(a.time) },
        ContainerCommand::Kill(a) => Command::Kill { ids: a.ids.clone(), signal: Some(a.signal.clone()) },
        ContainerCommand::Wait(a) => Command::Wait { ids: a.ids.clone() },
        ContainerCommand::Logs(a) => {
            Command::Logs { id: a.id.clone(), tail: a.tail, timestamps: a.timestamps, follow: a.follow }
        }
        ContainerCommand::Rm(a) => Command::Rm { ids: a.ids.clone(), force: a.force },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn paths() -> Paths<'static> {
        // a pretend current directory C:\cwd for relative paths
        Paths {
            distro: Some("Ubuntu-24.04"),
            absolute: |p| {
                let s = p.to_string_lossy();
                let absolute = s.starts_with(['/', '\\']) || s.as_bytes().get(1) == Some(&b':');
                Ok(if absolute { s.into_owned() } else { format!(r"C:\cwd\{s}") })
            },
        }
    }

    #[test]
    fn project_file_options_are_translated_for_the_linux_side() {
        let f = plan_of(&[
            "-f",
            r"C:\work\app\docker-compose.yml",
            "--env-file",
            r"C:\work\app\prod.env",
            "--env-file",
            "rel.env",
            "up",
            "-d",
        ])
        .unwrap();
        assert_eq!(f.globals.file.as_deref(), Some("/mnt/c/work/app/docker-compose.yml"));
        assert_eq!(f.globals.env_files, ["/mnt/c/work/app/prod.env", "/mnt/c/cwd/rel.env"]);
        // commands that do not read a project file send neither
        let f = plan_of(&["--env-file", "x.env", "image", "ls"]).unwrap();
        assert!(f.globals.env_files.is_empty());
    }

    #[test]
    fn the_default_project_file_is_resolved_against_the_current_directory() {
        let f = plan_of(&["down"]).unwrap();
        assert_eq!(f.globals.file.as_deref(), Some("/mnt/c/cwd/yarc.yaml"));
    }

    fn plan_of(args: &[&str]) -> Option<Forward> {
        let cli = Cli::try_parse_from(std::iter::once("yarc").chain(args.iter().copied())).unwrap();
        plan(&cli, &paths()).unwrap()
    }

    fn command(args: &[&str]) -> Command {
        plan_of(args).unwrap().command
    }

    #[test]
    fn project_commands_carry_the_translated_file_and_the_name() {
        let f = plan_of(&["--file", r"C:\work\app\yarc.yaml", "-p", "demo", "up", "-d"]).unwrap();
        assert_eq!(f.command, Command::Up { detach: true });
        assert_eq!(f.globals.file.as_deref(), Some("/mnt/c/work/app/yarc.yaml"));
        assert_eq!(f.globals.project.as_deref(), Some("demo"));
        assert_eq!(f.globals.root, None);
    }

    #[test]
    fn other_commands_do_not_send_the_project_file() {
        let f = plan_of(&["--file", r"C:\x\y.yaml", "ps"]).unwrap();
        assert_eq!(f.globals.file, None);
    }

    #[test]
    fn a_root_is_translated_and_a_missing_one_is_left_to_the_agent() {
        let f = plan_of(&["--root", r"\\wsl$\Ubuntu-24.04\home\me\store", "image", "ls"]).unwrap();
        assert_eq!(f.globals.root.as_deref(), Some("/home/me/store"));
    }

    #[test]
    fn path_arguments_are_translated() {
        assert_eq!(
            command(&["load", r"C:\bundle\images.tar"]),
            Command::Load { file: "/mnt/c/bundle/images.tar".into() }
        );
        assert_eq!(
            command(&["image", "unpack", "alpine", r"D:\rootfs"]),
            Command::ImageUnpack { image: "alpine".into(), dir: "/mnt/d/rootfs".into() }
        );
    }

    #[test]
    fn untranslatable_paths_fail_before_anything_is_sent() {
        let cli = Cli::try_parse_from(["yarc", "load", r"\\server\share\x.tar"]).unwrap();
        assert!(plan(&cli, &paths()).unwrap_err().to_string().contains("network path"));
    }

    #[test]
    fn run_and_create_carry_every_option() {
        let c = command(&[
            "run",
            "--rm",
            "-d",
            "--name",
            "w",
            "-e",
            "A=1",
            "-w",
            "/x",
            "-u",
            "root",
            "--network",
            "none",
            "alpine:3.20",
            "sh",
            "-c",
            "echo hi",
        ]);
        assert_eq!(
            c,
            Command::Run {
                spec: CreateSpec {
                    image: "alpine:3.20".into(),
                    name: Some("w".into()),
                    env: vec!["A=1".into()],
                    entrypoint: None,
                    workdir: Some("/x".into()),
                    user: Some("root".into()),
                    network: Some("none".into()),
                    restart: None,
                    memory: None,
                    memory_swap: None,
                    cpus: None,
                    pids_limit: None,
                    cmd: vec!["sh".into(), "-c".into(), "echo hi".into()],
                },
                rm: true,
                detach: true,
                debug: false,
            }
        );
        assert!(matches!(command(&["create", "alpine"]), Command::Create(s) if s.network.as_deref() == Some("host")));
        // a restart policy is forwarded as text; the default is not sent at all
        assert!(matches!(command(&["create", "alpine"]), Command::Create(s) if s.restart.is_none()));
        assert!(
            matches!(command(&["run", "-d", "--restart", "on-failure:3", "alpine"]), Command::Run { spec, .. } if spec.restart.as_deref() == Some("on-failure:3"))
        );
    }

    #[test]
    fn limits_are_forwarded_as_text_and_only_when_set() {
        assert!(matches!(command(&["create", "alpine"]), Command::Create(s)
            if (&s.memory, &s.memory_swap, &s.cpus, &s.pids_limit) == (&None, &None, &None, &None)));
        let run =
            command(&["run", "-m", "1.5g", "--memory-swap", "2g", "--cpus", "2.5", "--pids-limit", "50", "alpine"]);
        let Command::Run { spec, .. } = run else { panic!() };
        assert_eq!(spec.memory.as_deref(), Some("1610612736"));
        assert_eq!(spec.memory_swap.as_deref(), Some("2147483648"));
        assert_eq!((spec.cpus.as_deref(), spec.pids_limit.as_deref()), (Some("2.5"), Some("50")));
        // the text survives the agent's own parser
        assert_eq!(yarc_config::parse_memory(spec.memory.as_deref().unwrap()).unwrap(), 3 << 29);
        assert_eq!(yarc_config::parse_cpus(spec.cpus.as_deref().unwrap()).unwrap(), 2500);
    }

    #[test]
    fn container_commands_with_and_without_the_container_prefix() {
        let expected = Command::Stop { ids: vec!["a".into(), "b".into()], time: Some(3) };
        assert_eq!(command(&["stop", "-t", "3", "a", "b"]), expected);
        assert_eq!(command(&["container", "stop", "-t", "3", "a", "b"]), expected);
        assert_eq!(command(&["kill", "a"]), Command::Kill { ids: vec!["a".into()], signal: Some("KILL".into()) });
        assert_eq!(command(&["rm", "-f", "a"]), Command::Rm { ids: vec!["a".into()], force: true });
        assert_eq!(command(&["wait", "a"]), Command::Wait { ids: vec!["a".into()] });
        assert_eq!(command(&["container", "ls"]), Command::ContainerLs);
        assert_eq!(command(&["stats", "a"]), Command::Stats { ids: vec!["a".into()] });
        assert_eq!(
            command(&["logs", "--tail", "5", "-t", "-f", "a"]),
            Command::Logs { id: "a".into(), tail: Some(5), timestamps: true, follow: true }
        );
        assert_eq!(
            command(&["exec", "-e", "K=V", "-w", "/w", "a", "ls", "-l"]),
            Command::Exec {
                id: "a".into(),
                env: vec!["K=V".into()],
                workdir: Some("/w".into()),
                cmd: vec!["ls".into(), "-l".into()]
            }
        );
        assert_eq!(command(&["start", "-d", "a"]), Command::Start { id: "a".into(), detach: true, debug: false });
    }

    #[test]
    fn images_projects_and_defaults() {
        assert_eq!(command(&["pull", "nginx"]), Command::Pull { image: "nginx".into(), jobs: Some(4), platform: None });
        assert_eq!(command(&["image", "rm", "a:1"]), Command::ImageRm { images: vec!["a:1".into()] });
        assert_eq!(command(&["down", "-v"]), Command::Down { time: None, volumes: true });
        assert_eq!(command(&["project", "stop", "-t", "2"]), Command::ProjectStop { time: Some(2) });
        assert_eq!(
            command(&["project", "logs", "web", "-f"]),
            Command::ProjectLogs { services: vec!["web".into()], tail: None, timestamps: false, follow: true }
        );
        assert_eq!(command(&["ps", "-a"]), Command::Ps { all: true });
    }

    #[test]
    fn project_config_and_internal_commands_stay_local() {
        assert_eq!(plan_of(&["project", "config"]), None);
        assert_eq!(plan_of(&["doctor"]), None);
        assert_eq!(plan_of(&["agent", "--stdio"]), None);
    }
}
