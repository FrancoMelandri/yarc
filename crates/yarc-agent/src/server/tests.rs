use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use yarc_proto::{
    read_frame, write_frame, AgentFrame, ClientFrame, Command, CreateSpec, Event, Globals, ProtoError, RequestId,
    PROTOCOL_VERSION,
};

use super::serve;

/// A stand-in for the `yarc` binary: what it does depends on its command line.
const FAKE_YARC: &str = r#"#!/bin/sh
case "$*" in
  "project config") echo out; echo err >&2; exit 3;;
  "project ps") trap 'echo interrupted; exit 130' INT; echo started; while true; do sleep 0.1; done;;
  "image ls") kill -KILL $$;;
  "container ls") sleep 5 & echo hi; exit 0;;
  "down") printf '\342\234'; sleep 0.2; printf '\223\n';;
  *) echo "$@";;
esac
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    exe: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("yarc");
    std::fs::write(&exe, FAKE_YARC).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    Fixture { _dir: dir, exe }
}

struct Client {
    to_agent: UnixStream,
    from_agent: BufReader<UnixStream>,
    server: JoinHandle<Result<(), ProtoError>>,
}

fn connect(exe: &Path) -> Client {
    let (client, agent) = UnixStream::pair().unwrap();
    client.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let agent_out = agent.try_clone().unwrap();
    let exe = exe.to_path_buf();
    let server = std::thread::spawn(move || serve(BufReader::new(agent), agent_out, &exe));
    Client { from_agent: BufReader::new(client.try_clone().unwrap()), to_agent: client, server }
}

impl Client {
    fn send(&mut self, frame: &ClientFrame) {
        write_frame(&mut self.to_agent, frame).unwrap();
    }

    fn hello(&mut self) {
        self.send(&ClientFrame::Hello { version: PROTOCOL_VERSION, client: "test".into() });
        assert!(matches!(self.recv(), Some(AgentFrame::Hello { version: PROTOCOL_VERSION, .. })));
    }

    fn recv(&mut self) -> Option<AgentFrame> {
        read_frame(&mut self.from_agent).expect("a valid frame or end of stream")
    }

    fn request(&mut self, id: RequestId, command: Command) {
        self.send(&ClientFrame::Request { id, globals: Globals::default(), command });
    }

    /// Everything request `id` produced up to and including its last event.
    fn finish(&mut self, id: RequestId) -> (String, String, Event) {
        let (mut out, mut err) = (String::new(), String::new());
        loop {
            match self.recv().expect("the stream ended before the request finished") {
                AgentFrame::Event { id: got, event } if got == id => match event {
                    Event::Stdout { text } => out.push_str(&text),
                    Event::Stderr { text } => err.push_str(&text),
                    last @ (Event::Exit { .. } | Event::Failed { .. }) => return (out, err, last),
                },
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    /// Closes the client's side and returns the server's result.
    fn close(self) -> Result<(), ProtoError> {
        self.to_agent.shutdown(std::net::Shutdown::Write).unwrap();
        self.server.join().unwrap()
    }
}

#[test]
fn output_and_exit_status_become_events() {
    let f = fixture();
    let mut c = connect(&f.exe);
    c.hello();
    c.request(1, Command::ProjectConfig);
    let (out, err, last) = c.finish(1);
    assert_eq!((out.as_str(), err.as_str(), last), ("out\n", "err\n", Event::Exit { code: 3 }));
    c.close().unwrap();
}

#[test]
fn the_command_line_comes_from_the_request() {
    let f = fixture();
    let mut c = connect(&f.exe);
    c.hello();
    c.send(&ClientFrame::Request {
        id: 5,
        globals: Globals { root: Some("/r".into()), ..Default::default() },
        command: Command::Ps { all: true },
    });
    let (out, _, last) = c.finish(5);
    assert_eq!((out.as_str(), last), ("--root=/r ps --all\n", Event::Exit { code: 0 }));
    c.close().unwrap();
}

#[test]
fn a_cancel_is_sigint_to_that_request_only() {
    let f = fixture();
    let mut c = connect(&f.exe);
    c.hello();
    c.request(1, Command::ProjectPs);
    c.request(2, Command::ProjectPs);
    // wait until both are really running
    let mut started = 0;
    while started < 2 {
        if let Some(AgentFrame::Event { event: Event::Stdout { .. }, .. }) = c.recv() {
            started += 1;
        }
    }
    c.send(&ClientFrame::Cancel { id: 1 });
    let (out, _, last) = c.finish(1);
    assert_eq!((out.as_str(), last), ("interrupted\n", Event::Exit { code: 130 }));
    c.send(&ClientFrame::Cancel { id: 2 });
    let (_, _, last) = c.finish(2);
    assert_eq!(last, Event::Exit { code: 130 });
    c.send(&ClientFrame::Cancel { id: 99 }); // unknown ids are ignored
    c.close().unwrap();
}

#[test]
fn closing_the_stream_interrupts_what_still_runs() {
    let f = fixture();
    let mut c = connect(&f.exe);
    c.hello();
    c.request(1, Command::ProjectPs);
    while !matches!(c.recv(), Some(AgentFrame::Event { event: Event::Stdout { .. }, .. })) {}
    let begun = Instant::now();
    c.close().unwrap();
    assert!(begun.elapsed() < Duration::from_secs(4), "the child was interrupted, not waited for");
}

#[test]
fn requests_run_concurrently_and_their_events_are_told_apart() {
    let f = fixture();
    let mut c = connect(&f.exe);
    c.hello();
    c.request(1, Command::ProjectConfig);
    c.request(2, Command::ProjectConfig);
    let mut done = std::collections::BTreeSet::new();
    while done.len() < 2 {
        if let Some(AgentFrame::Event { id, event: Event::Exit { code } }) = c.recv() {
            assert_eq!(code, 3);
            done.insert(id);
        }
    }
    assert_eq!(done.into_iter().collect::<Vec<_>>(), [1, 2]);
    c.close().unwrap();
}

#[test]
fn a_request_id_in_use_is_refused_and_the_first_request_is_untouched() {
    let f = fixture();
    let mut c = connect(&f.exe);
    c.hello();
    c.request(1, Command::ProjectPs);
    c.request(1, Command::ProjectConfig);
    let mut refused = false;
    let mut started = false;
    while !(refused && started) {
        match c.recv().unwrap() {
            AgentFrame::Event { id: 1, event: Event::Failed { message, code: 125 } } => {
                assert!(message.contains("already running"), "{message}");
                refused = true;
            }
            AgentFrame::Event { id: 1, event: Event::Stdout { .. } } => started = true,
            other => panic!("unexpected {other:?}"),
        }
    }
    c.send(&ClientFrame::Cancel { id: 1 });
    assert_eq!(c.finish(1).2, Event::Exit { code: 130 });
    c.close().unwrap();
}

#[test]
fn a_command_that_cannot_be_built_or_started_fails_with_125() {
    let f = fixture();
    let mut c = connect(&f.exe);
    c.hello();
    c.request(1, Command::Wait { ids: vec![] });
    let (_, _, last) = c.finish(1);
    assert!(matches!(&last, Event::Failed { code: 125, message } if message.contains("ids")), "{last:?}");
    c.close().unwrap();

    let mut c = connect(&f.exe.with_file_name("missing"));
    c.hello();
    c.request(1, Command::ImageLs);
    let (_, _, last) = c.finish(1);
    assert!(matches!(&last, Event::Failed { code: 125, message } if message.contains("cannot run")), "{last:?}");
    c.close().unwrap();
}

#[test]
fn a_signalled_command_exits_with_128_plus_the_signal() {
    let f = fixture();
    let mut c = connect(&f.exe);
    c.hello();
    c.request(1, Command::ImageLs);
    assert_eq!(c.finish(1).2, Event::Exit { code: 128 + 9 });
    c.close().unwrap();
}

#[test]
fn a_grandchild_holding_the_pipes_does_not_delay_the_exit() {
    let f = fixture();
    let mut c = connect(&f.exe);
    c.hello();
    let begun = Instant::now();
    c.request(1, Command::ContainerLs); // leaves `sleep 5` running with our pipes
    let (out, _, last) = c.finish(1);
    assert_eq!((out.as_str(), last), ("hi\n", Event::Exit { code: 0 }));
    assert!(begun.elapsed() < Duration::from_secs(3), "{:?}", begun.elapsed());
    c.close().unwrap();
}

#[test]
fn a_character_split_between_reads_arrives_whole() {
    let f = fixture();
    let mut c = connect(&f.exe);
    c.hello();
    c.request(1, Command::Down { time: None, volumes: false });
    let (out, _, _) = c.finish(1);
    assert_eq!(out, "✓\n");
    c.close().unwrap();
}

#[test]
fn the_handshake_is_enforced() {
    let f = fixture();

    let mut c = connect(&f.exe);
    c.send(&ClientFrame::Hello { version: PROTOCOL_VERSION + 1, client: "old".into() });
    assert!(matches!(c.recv(), Some(AgentFrame::Fatal { message }) if message.contains("mismatch")));
    assert!(matches!(c.server.join().unwrap(), Err(ProtoError::VersionMismatch { .. })));

    let mut c = connect(&f.exe);
    c.request(1, Command::ImageLs);
    assert!(matches!(c.recv(), Some(AgentFrame::Fatal { .. })));
    assert!(c.server.join().unwrap().is_err());

    let mut c = connect(&f.exe);
    c.hello();
    c.send(&ClientFrame::Hello { version: PROTOCOL_VERSION, client: "again".into() });
    assert!(matches!(c.recv(), Some(AgentFrame::Fatal { .. })));
    assert!(c.server.join().unwrap().is_err());
}

#[test]
fn garbage_ends_the_session_with_a_fatal_frame_and_an_empty_stream_is_fine() {
    use std::io::Write;
    let f = fixture();
    let mut c = connect(&f.exe);
    c.hello();
    c.to_agent.write_all(b"this is not json\n").unwrap();
    assert!(matches!(c.recv(), Some(AgentFrame::Fatal { .. })));
    assert!(matches!(c.server.join().unwrap(), Err(ProtoError::Malformed(_))));

    let c = connect(&f.exe);
    c.close().unwrap();
}

#[test]
fn create_options_reach_the_command_line() {
    let f = fixture();
    let mut c = connect(&f.exe);
    c.hello();
    let spec = CreateSpec { image: "alpine".into(), cmd: vec!["echo".into(), "hi".into()], ..Default::default() };
    c.request(1, Command::Create(spec));
    let (out, _, _) = c.finish(1);
    assert_eq!(out, "create -- alpine echo hi\n");
    c.close().unwrap();
}
