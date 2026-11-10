//! Real containers through the agent: needs user namespaces (the image comes
//! from the shared test cache), so run explicitly with
//! `cargo test -p yarc-cli --test live_agent -- --ignored --test-threads=1`.

#![cfg(target_os = "linux")]

mod common;

use std::io::BufReader;
use std::process::{Child, ChildStdin, ChildStdout, Command as Process, Stdio};
use std::time::{Duration, Instant};

use yarc_proto::{
    read_frame, write_frame, AgentFrame, ClientFrame, Command, CreateSpec, Event, Globals, PROTOCOL_VERSION,
};

struct Agent {
    child: Child,
    stdin: ChildStdin,
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
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut a = Agent { child, stdin, stdout };
    a.send(&ClientFrame::Hello { version: PROTOCOL_VERSION, client: "live".into() });
    assert!(matches!(a.recv(), AgentFrame::Hello { .. }));
    a
}

impl Agent {
    fn send(&mut self, frame: &ClientFrame) {
        write_frame(&mut self.stdin, frame).unwrap();
    }

    fn recv(&mut self) -> AgentFrame {
        read_frame(&mut self.stdout).unwrap().expect("the agent ended the stream")
    }

    fn request(&mut self, id: u64, globals: &Globals, command: Command) {
        self.send(&ClientFrame::Request { id, globals: globals.clone(), command });
    }

    /// Reads events of request `id` until `stop` says so on the accumulated
    /// stderr, or the request ends; returns (stdout, stderr, exit code if ended).
    fn read_until(&mut self, id: u64, stop: impl Fn(&str) -> bool) -> (String, String, Option<i32>) {
        let (mut out, mut err) = (String::new(), String::new());
        let give_up = Instant::now() + Duration::from_secs(60);
        while Instant::now() < give_up {
            match self.recv() {
                AgentFrame::Event { id: got, event } if got == id => match event {
                    Event::Stdout { text } => out.push_str(&text),
                    Event::Stderr { text } => {
                        err.push_str(&text);
                        if stop(&err) {
                            return (out, err, None);
                        }
                    }
                    Event::Exit { code } => return (out, err, Some(code)),
                    Event::Failed { message, .. } => panic!("failed: {message}"),
                },
                other => panic!("unexpected {other:?}"),
            }
        }
        panic!("timed out; stdout {out:?} stderr {err:?}");
    }

    fn finish(mut self) {
        drop(self.stdin);
        assert!(self.child.wait().unwrap().success());
    }
}

#[test]
#[ignore]
fn run_streams_a_containers_output_through_the_agent() {
    let root = common::seeded_root();
    let globals = Globals { root: Some(root.path().display().to_string()), ..Default::default() };
    let mut a = agent();
    let spec = CreateSpec { image: "alpine:3.20".into(), cmd: vec!["sh".into(), "-c".into(), "echo hi; echo oops >&2; exit 4".into()], ..Default::default() };
    a.request(1, &globals, Command::Run { spec, rm: true, detach: false, debug: false });
    let (out, err, code) = a.read_until(1, |_| false);
    assert_eq!((out.as_str(), err.as_str(), code), ("hi\n", "oops\n", Some(4)));
    a.finish();
}

#[test]
#[ignore]
fn cancel_stops_a_foreground_up_like_ctrl_c() {
    let root = common::seeded_root();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("yarc.yaml");
    std::fs::write(&file, "services:\n  one:\n    image: alpine:3.20\n    cmd: [sh, -c, 'while true; do sleep 0.2; done']\n").unwrap();
    let globals = Globals {
        root: Some(root.path().display().to_string()),
        file: Some(file.display().to_string()),
        project: Some("agentlive".into()),
        ..Default::default()
    };
    let mut a = agent();

    a.request(1, &globals, Command::Up { detach: false });
    // `up` reports each service on stderr; wait until it is started
    a.read_until(1, |err| err.contains("started") || err.contains("created and started"));
    a.send(&ClientFrame::Cancel { id: 1 });
    let (_, err, code) = a.read_until(1, |_| false);
    assert_eq!(code, Some(130), "{err}");
    assert!(err.contains("Gracefully stopping"), "{err}");

    a.request(2, &globals, Command::Down { time: Some(2), volumes: false });
    let (_, err, code) = a.read_until(2, |_| false);
    assert_eq!(code, Some(0), "{err}");
    a.finish();
}
