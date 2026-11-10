use std::io::Cursor;

use crate::*;

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap()
}

fn spec() -> CreateSpec {
    CreateSpec { image: "alpine:3.20".into(), cmd: vec!["echo".into(), "hi".into()], ..Default::default() }
}

fn every_command() -> Vec<Command> {
    vec![
        Command::Pull { image: "a".into(), jobs: Some(8), platform: Some("linux/arm64".into()) },
        Command::Load { file: "/x/images.tar".into() },
        Command::ImageLs,
        Command::ImageRm { images: vec!["a:1".into()] },
        Command::ImageUnpack { image: "a".into(), dir: "/d".into() },
        Command::Create(spec()),
        Command::Run { spec: spec(), rm: true, detach: true, debug: false },
        Command::Ps { all: true },
        Command::ContainerLs,
        Command::Start { id: "c".into(), detach: true, debug: true },
        Command::Exec { id: "c".into(), env: vec!["A=1".into()], workdir: Some("/w".into()), cmd: vec!["ls".into()] },
        Command::Stop { ids: vec!["c".into()], time: Some(3) },
        Command::Kill { ids: vec!["c".into()], signal: Some("TERM".into()) },
        Command::Wait { ids: vec!["c".into()] },
        Command::Logs { id: "c".into(), tail: Some(5), timestamps: true, follow: true },
        Command::Rm { ids: vec!["c".into()], force: true },
        Command::Up { detach: true },
        Command::Down { time: Some(1), volumes: true },
        Command::ProjectPs,
        Command::ProjectLogs { services: vec!["web".into()], tail: None, timestamps: false, follow: true },
        Command::ProjectStop { time: None },
        Command::ProjectConfig,
    ]
}

#[test]
fn every_command_survives_a_round_trip() {
    for command in every_command() {
        let frame = ClientFrame::Request { id: 1, globals: Globals::default(), command };
        let back: ClientFrame = serde_json::from_str(&json(&frame)).unwrap();
        assert_eq!(back, frame);
    }
}

/// The wire format is a contract: changing these strings needs a new PROTOCOL_VERSION.
#[test]
fn golden_wire_format() {
    assert_eq!(
        json(&ClientFrame::Hello { version: 1, client: "yarc.exe 0.0.1".into() }),
        r#"{"type":"hello","version":1,"client":"yarc.exe 0.0.1"}"#
    );
    assert_eq!(
        json(&AgentFrame::Hello { version: 1, agent: "yarc 0.0.1".into() }),
        r#"{"type":"hello","version":1,"agent":"yarc 0.0.1"}"#
    );
    assert_eq!(json(&ClientFrame::Cancel { id: 7 }), r#"{"type":"cancel","id":7}"#);
    assert_eq!(
        json(&ClientFrame::Request {
            id: 2,
            globals: Globals { root: None, file: Some("/p/yarc.yaml".into()), project: Some("demo".into()), profiles: vec![] },
            command: Command::Up { detach: true },
        }),
        r#"{"type":"request","id":2,"globals":{"root":null,"file":"/p/yarc.yaml","project":"demo"},"command":{"type":"up","detach":true}}"#
    );
    assert_eq!(
        json(&AgentFrame::Event { id: 2, event: Event::Stdout { text: "hi\n".into() } }),
        r#"{"type":"event","id":2,"event":{"type":"stdout","text":"hi\n"}}"#
    );
    assert_eq!(
        json(&AgentFrame::Event { id: 2, event: Event::Failed { message: "boom".into(), code: 125 } }),
        r#"{"type":"event","id":2,"event":{"type":"failed","message":"boom","code":125}}"#
    );
    assert_eq!(json(&AgentFrame::Fatal { message: "x".into() }), r#"{"type":"fatal","message":"x"}"#);
    assert_eq!(json(&Command::ProjectConfig), r#"{"type":"project_config"}"#);
}

#[test]
fn run_flattens_the_create_options() {
    let run = Command::Run { spec: spec(), rm: true, detach: false, debug: false };
    let value: serde_json::Value = serde_json::from_str(&json(&run)).unwrap();
    assert_eq!(value["type"], "run");
    assert_eq!(value["image"], "alpine:3.20");
    assert_eq!(value["rm"], true);
}

#[test]
fn optional_fields_default_and_unknown_fields_are_ignored() {
    let c: Command = serde_json::from_str(r#"{"type":"down","future":"x"}"#).unwrap();
    assert_eq!(c, Command::Down { time: None, volumes: false });
    let c: Command = serde_json::from_str(r#"{"type":"run","image":"a"}"#).unwrap();
    assert_eq!(c, Command::Run { spec: CreateSpec { image: "a".into(), ..Default::default() }, rm: false, detach: false, debug: false });
    let g: Globals = serde_json::from_str("{}").unwrap();
    assert_eq!(g, Globals::default());
}

#[test]
fn unknown_commands_and_frames_are_errors() {
    assert!(serde_json::from_str::<Command>(r#"{"type":"reboot"}"#).is_err());
    assert!(serde_json::from_str::<ClientFrame>(r#"{"type":"nope"}"#).is_err());
    assert!(serde_json::from_str::<Command>(r#"{"type":"load"}"#).is_err(), "required field missing");
}

#[test]
fn frames_are_one_line_each_and_read_back_in_order() {
    let mut wire = Vec::new();
    write_frame(&mut wire, &AgentFrame::Event { id: 1, event: Event::Stdout { text: "a\nb".into() } }).unwrap();
    write_frame(&mut wire, &AgentFrame::Event { id: 1, event: Event::Exit { code: 0 } }).unwrap();
    let text = String::from_utf8(wire.clone()).unwrap();
    assert_eq!(text.lines().count(), 2, "newlines inside text are escaped: {text}");

    let mut input = Cursor::new(wire);
    let first: AgentFrame = read_frame(&mut input).unwrap().unwrap();
    assert_eq!(first, AgentFrame::Event { id: 1, event: Event::Stdout { text: "a\nb".into() } });
    let second: AgentFrame = read_frame(&mut input).unwrap().unwrap();
    assert_eq!(second, AgentFrame::Event { id: 1, event: Event::Exit { code: 0 } });
    assert!(read_frame::<_, AgentFrame>(&mut input).unwrap().is_none(), "clean end of input");
}

#[test]
fn blank_lines_are_skipped_and_a_last_frame_needs_no_newline() {
    let mut input = Cursor::new(b"\n  \n{\"type\":\"cancel\",\"id\":3}".to_vec());
    assert_eq!(read_frame::<_, ClientFrame>(&mut input).unwrap(), Some(ClientFrame::Cancel { id: 3 }));
}

#[test]
fn garbage_is_malformed() {
    for bad in ["not json\n", "{\"type\":\"cancel\"}\n", "\u{0}\n"] {
        let r = read_frame::<_, ClientFrame>(&mut Cursor::new(bad.as_bytes().to_vec()));
        assert!(matches!(r, Err(ProtoError::Malformed(_))), "{bad:?}");
    }
    let r = read_frame::<_, ClientFrame>(&mut Cursor::new(vec![0xff, 0xfe, b'\n']));
    assert!(matches!(r, Err(ProtoError::Malformed(_))), "invalid utf-8");
}

#[test]
fn oversized_frames_are_refused_on_both_sides() {
    let big = "x".repeat(MAX_FRAME_BYTES);
    let mut sink = Vec::new();
    let frame = AgentFrame::Event { id: 1, event: Event::Stdout { text: big.clone() } };
    assert!(matches!(write_frame(&mut sink, &frame), Err(ProtoError::TooLarge)));
    assert!(sink.is_empty(), "nothing partial is written");

    let mut line = vec![b'a'; MAX_FRAME_BYTES + 10];
    line.push(b'\n');
    assert!(matches!(read_frame::<_, ClientFrame>(&mut Cursor::new(line)), Err(ProtoError::TooLarge)));
}

#[test]
fn versions_must_match() {
    assert!(check_version(PROTOCOL_VERSION).is_ok());
    let err = check_version(PROTOCOL_VERSION + 1).unwrap_err();
    assert!(matches!(err, ProtoError::VersionMismatch { .. }));
    assert!(err.to_string().contains("mismatch"));
}
