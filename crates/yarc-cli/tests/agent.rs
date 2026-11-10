//! `yarc agent --stdio` against the real binary. Needs no network and no
//! user namespaces: it only uses commands that read the store and the project file.

#![cfg(target_os = "linux")]

use std::io::BufReader;
use std::process::{Child, ChildStdin, ChildStdout, Command as Process, Stdio};

use yarc_proto::{read_frame, write_frame, AgentFrame, ClientFrame, Command, Event, Globals, PROTOCOL_VERSION};

struct Agent {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

fn agent() -> Agent {
    let mut child = Process::new(env!("CARGO_BIN_EXE_yarc"))
        .args(["agent", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut a = Agent { child, stdin, stdout };
    a.send(&ClientFrame::Hello { version: PROTOCOL_VERSION, client: "test".into() });
    assert!(matches!(a.recv(), Some(AgentFrame::Hello { .. })));
    a
}

impl Agent {
    fn send(&mut self, frame: &ClientFrame) {
        write_frame(self.stdin.as_mut().unwrap(), frame).unwrap();
    }

    fn recv(&mut self) -> Option<AgentFrame> {
        read_frame(&mut self.stdout).unwrap()
    }

    /// Runs one request to its end; returns (stdout, stderr, exit code).
    fn run(&mut self, id: u64, globals: Globals, command: Command) -> (String, String, i32) {
        self.send(&ClientFrame::Request { id, globals, command });
        let (mut out, mut err) = (String::new(), String::new());
        loop {
            match self.recv().expect("stream ended early") {
                AgentFrame::Event { id: got, event } if got == id => match event {
                    Event::Stdout { text } => out.push_str(&text),
                    Event::Stderr { text } => err.push_str(&text),
                    Event::Exit { code } => return (out, err, code),
                    Event::Failed { message, .. } => panic!("failed: {message}"),
                },
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    fn finish(mut self) {
        drop(self.stdin.take());
        assert!(self.child.wait().unwrap().success(), "the agent exits cleanly when its input ends");
    }
}

#[test]
fn requests_run_the_real_cli_and_report_its_output_and_status() {
    let root = tempfile::tempdir().unwrap();
    let globals = Globals { root: Some(root.path().display().to_string()), ..Default::default() };
    let mut a = agent();

    let (out, err, code) = a.run(1, globals.clone(), Command::ImageLs);
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.starts_with("IMAGE"), "{out}");

    // the CLI's own errors travel as stderr text with its exit status
    let (out, err, code) = a.run(2, globals.clone(), Command::ImageRm { images: vec!["nope:1".into()] });
    assert_eq!((out.as_str(), code), ("", 1));
    assert!(err.contains("not in the local store"), "{err}");

    // options with values that look like options stay values
    let (_, err, code) = a.run(3, globals, Command::ImageRm { images: vec!["--root=/etc".into()] });
    assert_eq!(code, 1);
    assert!(err.contains("--root=/etc"), "{err}");
    a.finish();
}

#[test]
fn project_commands_use_the_requested_file_and_name() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("yarc.yaml");
    std::fs::write(&file, "services:\n  web:\n    image: alpine:3.20\n    cmd: [sleep, '1']\n").unwrap();
    let globals =
        Globals { file: Some(file.display().to_string()), project: Some("agentdemo".into()), ..Default::default() };
    let mut a = agent();
    let (out, err, code) = a.run(1, globals, Command::ProjectConfig);
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.contains("project: agentdemo") && out.contains("web"), "{out}");
    a.finish();
}
