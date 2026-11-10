//! `Command` -> the command line of the equivalent `yarc` invocation.
//!
//! Options are always written in their `--name=value` form and positionals
//! come after a `--`, so no value a client sends can be taken for an option
//! (an image called `--root` stays an image name).

use std::fmt;

use yarc_proto::{Command, CreateSpec, Globals};

#[derive(Debug, PartialEq, Eq)]
pub struct ArgError(pub String);

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ArgError {}

/// Collects arguments: flags, `--opt=value` pairs and trailing positionals.
#[derive(Default)]
struct Args {
    words: Vec<String>,
    positionals: Vec<String>,
}

impl Args {
    fn word(mut self, w: &str) -> Self {
        self.words.push(w.to_string());
        self
    }

    fn flag(mut self, on: bool, name: &str) -> Self {
        if on {
            self.words.push(name.to_string());
        }
        self
    }

    fn opt<T: ToString>(mut self, name: &str, value: Option<T>) -> Self {
        if let Some(v) = value {
            self.words.push(format!("{name}={}", v.to_string()));
        }
        self
    }

    fn opts(mut self, name: &str, values: &[String]) -> Self {
        self.words.extend(values.iter().map(|v| format!("{name}={v}")));
        self
    }

    fn pos(mut self, values: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.positionals.extend(values.into_iter().map(Into::into));
        self
    }

    fn finish(self) -> Vec<String> {
        let mut out = self.words;
        if !self.positionals.is_empty() {
            out.push("--".into());
            out.extend(self.positionals);
        }
        out
    }
}

fn create_opts(a: Args, s: &CreateSpec) -> Args {
    a.opt("--name", s.name.as_deref())
        .opts("--env", &s.env)
        .opt("--entrypoint", s.entrypoint.as_deref())
        .opt("--workdir", s.workdir.as_deref())
        .opt("--user", s.user.as_deref())
        .opt("--network", s.network.as_deref())
        .opt("--restart", s.restart.as_deref())
}

fn create_positionals(a: Args, s: &CreateSpec) -> Args {
    a.pos([s.image.clone()]).pos(s.cmd.clone())
}

/// The arguments to give the `yarc` binary (without its own name).
pub fn to_args(globals: &Globals, command: &Command) -> Result<Vec<String>, ArgError> {
    let mut out = Vec::new();
    if let Some(root) = &globals.root {
        out.push(format!("--root={root}"));
    }
    if let Some(file) = &globals.file {
        out.push(format!("--file={file}"));
    }
    for profile in &globals.profiles {
        out.push(format!("--profile={profile}"));
    }
    for env_file in &globals.env_files {
        out.push(format!("--env-file={env_file}"));
    }
    if let Some(project) = &globals.project {
        out.push(format!("--project={project}"));
    }
    out.extend(command_args(command)?);
    Ok(out)
}

fn need(name: &str, values: &[String]) -> Result<(), ArgError> {
    if values.is_empty() {
        Err(ArgError(format!("{name}: at least one is required")))
    } else {
        Ok(())
    }
}

fn command_args(command: &Command) -> Result<Vec<String>, ArgError> {
    let a = Args::default();
    Ok(match command {
        Command::Pull { image, jobs, platform } => {
            a.word("pull").opt("--jobs", *jobs).opt("--platform", platform.as_deref()).pos([image.clone()]).finish()
        }
        Command::Load { file } => a.word("load").pos([file.clone()]).finish(),
        Command::ImageLs => a.word("image").word("ls").finish(),
        Command::ImageRm { images } => {
            need("images", images)?;
            a.word("image").word("rm").pos(images.clone()).finish()
        }
        Command::ImageUnpack { image, dir } => a.word("image").word("unpack").pos([image.clone(), dir.clone()]).finish(),
        Command::Create(spec) => create_positionals(create_opts(a.word("create"), spec), spec).finish(),
        Command::Run { spec, rm, detach, debug } => {
            let a = a.word("run").flag(*rm, "--rm").flag(*detach, "--detach").flag(*debug, "--debug");
            create_positionals(create_opts(a, spec), spec).finish()
        }
        Command::Ps { all } => a.word("ps").flag(*all, "--all").finish(),
        Command::ContainerLs => a.word("container").word("ls").finish(),
        Command::Start { id, detach, debug } => {
            a.word("container").word("start").flag(*detach, "--detach").flag(*debug, "--debug").pos([id.clone()]).finish()
        }
        Command::Exec { id, env, workdir, cmd } => {
            need("cmd", cmd)?;
            a.word("container")
                .word("exec")
                .opts("--env", env)
                .opt("--workdir", workdir.as_deref())
                .pos([id.clone()])
                .pos(cmd.clone())
                .finish()
        }
        Command::Stop { ids, time } => {
            need("ids", ids)?;
            a.word("container").word("stop").opt("--time", *time).pos(ids.clone()).finish()
        }
        Command::Kill { ids, signal } => {
            need("ids", ids)?;
            a.word("container").word("kill").opt("--signal", signal.as_deref()).pos(ids.clone()).finish()
        }
        Command::Wait { ids } => {
            need("ids", ids)?;
            a.word("container").word("wait").pos(ids.clone()).finish()
        }
        Command::Logs { id, tail, timestamps, follow } => a
            .word("container")
            .word("logs")
            .opt("--tail", *tail)
            .flag(*timestamps, "--timestamps")
            .flag(*follow, "--follow")
            .pos([id.clone()])
            .finish(),
        Command::Rm { ids, force } => {
            need("ids", ids)?;
            a.word("container").word("rm").flag(*force, "--force").pos(ids.clone()).finish()
        }
        Command::Up { detach } => a.word("up").flag(*detach, "--detach").finish(),
        Command::Down { time, volumes } => a.word("down").opt("--time", *time).flag(*volumes, "--volumes").finish(),
        Command::ProjectPs => a.word("project").word("ps").finish(),
        Command::ProjectLogs { services, tail, timestamps, follow } => a
            .word("project")
            .word("logs")
            .opt("--tail", *tail)
            .flag(*timestamps, "--timestamps")
            .flag(*follow, "--follow")
            .pos(services.clone())
            .finish(),
        Command::ProjectStop { time } => a.word("project").word("stop").opt("--time", *time).finish(),
        Command::ProjectConfig => a.word("project").word("config").finish(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(command: Command) -> Vec<String> {
        to_args(&Globals::default(), &command).unwrap()
    }

    fn spec(image: &str, cmd: &[&str]) -> CreateSpec {
        CreateSpec { image: image.into(), cmd: cmd.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn globals_come_first() {
        let g = Globals { root: Some("/r".into()), file: Some("/p/y.yaml".into()), project: Some("demo".into()), profiles: vec!["obs".into(), "dbg".into()], env_files: vec!["/p/.env.prod".into()] };
        assert_eq!(
            to_args(&g, &Command::Up { detach: true }).unwrap(),
            ["--root=/r", "--file=/p/y.yaml", "--profile=obs", "--profile=dbg", "--env-file=/p/.env.prod", "--project=demo", "up", "--detach"]
        );
        assert_eq!(to_args(&Globals::default(), &Command::ProjectConfig).unwrap(), ["project", "config"]);
    }

    #[test]
    fn positionals_follow_a_double_dash_so_they_can_never_be_options() {
        assert_eq!(args(Command::Load { file: "--root=/x".into() }), ["load", "--", "--root=/x"]);
        assert_eq!(args(Command::Pull { image: "-h".into(), jobs: Some(2), platform: None }), ["pull", "--jobs=2", "--", "-h"]);
    }

    #[test]
    fn run_keeps_the_command_and_its_dashes_after_the_image() {
        let mut s = spec("alpine:3.20", &["sh", "-c", "echo hi", "--version"]);
        s.name = Some("web".into());
        s.env = vec!["A=1".into(), "B=-2".into()];
        s.entrypoint = Some(String::new());
        s.network = Some("none".into());
        assert_eq!(
            args(Command::Run { spec: s, rm: true, detach: false, debug: true }),
            [
                "run", "--rm", "--debug", "--name=web", "--env=A=1", "--env=B=-2", "--entrypoint=", "--network=none", "--",
                "alpine:3.20", "sh", "-c", "echo hi", "--version"
            ]
        );
    }

    #[test]
    fn create_start_exec() {
        assert_eq!(args(Command::Create(spec("a", &[]))), ["create", "--", "a"]);
        assert_eq!(
            args(Command::Start { id: "c".into(), detach: true, debug: false }),
            ["container", "start", "--detach", "--", "c"]
        );
        assert_eq!(
            args(Command::Exec { id: "c".into(), env: vec!["X=1".into()], workdir: Some("/w".into()), cmd: vec!["ls".into(), "-l".into()] }),
            ["container", "exec", "--env=X=1", "--workdir=/w", "--", "c", "ls", "-l"]
        );
    }

    #[test]
    fn container_lifecycle_commands() {
        assert_eq!(
            args(Command::Stop { ids: vec!["a".into(), "b".into()], time: Some(3) }),
            ["container", "stop", "--time=3", "--", "a", "b"]
        );
        assert_eq!(
            args(Command::Kill { ids: vec!["a".into()], signal: Some("TERM".into()) }),
            ["container", "kill", "--signal=TERM", "--", "a"]
        );
        assert_eq!(args(Command::Wait { ids: vec!["a".into()] }), ["container", "wait", "--", "a"]);
        assert_eq!(args(Command::Rm { ids: vec!["a".into()], force: true }), ["container", "rm", "--force", "--", "a"]);
        assert_eq!(
            args(Command::Logs { id: "a".into(), tail: Some(5), timestamps: true, follow: true }),
            ["container", "logs", "--tail=5", "--timestamps", "--follow", "--", "a"]
        );
        assert_eq!(args(Command::Ps { all: true }), ["ps", "--all"]);
        assert_eq!(args(Command::ContainerLs), ["container", "ls"]);
    }

    #[test]
    fn a_restart_policy_reaches_the_command_line_only_when_set() {
        let run = |restart: Option<&str>| {
            let spec = CreateSpec { restart: restart.map(String::from), ..spec("alpine", &["true"]) };
            args(Command::Run { spec, rm: false, detach: true, debug: false })
        };
        assert!(!run(None).iter().any(|a| a.starts_with("--restart")), "{:?}", run(None));
        let with = run(Some("on-failure:3"));
        assert!(with.contains(&"--restart=on-failure:3".to_string()), "{with:?}");
        // before the image, so it can never be taken for part of the container's command
        let at = with.iter().position(|a| a.starts_with("--restart")).unwrap();
        assert!(at < with.iter().position(|a| a == "alpine").unwrap());
    }

    #[test]
    fn images_and_projects() {
        assert_eq!(args(Command::ImageLs), ["image", "ls"]);
        assert_eq!(args(Command::ImageRm { images: vec!["a:1".into()] }), ["image", "rm", "--", "a:1"]);
        assert_eq!(args(Command::ImageUnpack { image: "a".into(), dir: "/d".into() }), ["image", "unpack", "--", "a", "/d"]);
        assert_eq!(args(Command::Down { time: Some(1), volumes: true }), ["down", "--time=1", "--volumes"]);
        assert_eq!(args(Command::ProjectPs), ["project", "ps"]);
        assert_eq!(args(Command::ProjectStop { time: Some(2) }), ["project", "stop", "--time=2"]);
        assert_eq!(
            args(Command::ProjectLogs { services: vec!["web".into()], tail: None, timestamps: false, follow: true }),
            ["project", "logs", "--follow", "--", "web"]
        );
    }

    #[test]
    fn missing_required_lists_are_rejected_before_anything_runs() {
        for c in [
            Command::ImageRm { images: vec![] },
            Command::Stop { ids: vec![], time: None },
            Command::Kill { ids: vec![], signal: None },
            Command::Wait { ids: vec![] },
            Command::Rm { ids: vec![], force: false },
            Command::Exec { id: "c".into(), env: vec![], workdir: None, cmd: vec![] },
        ] {
            assert!(to_args(&Globals::default(), &c).is_err(), "{c:?}");
        }
    }
}
