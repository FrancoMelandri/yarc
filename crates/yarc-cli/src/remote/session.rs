//! One request against an agent: handshake, send, relay events until the end.

use std::io::{BufRead, Write};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Result};
use yarc_proto::{
    check_version, read_frame, write_frame, AgentFrame, ClientFrame, Event, ProtoError, PROTOCOL_VERSION,
};

use super::forward::Forward;

const REQUEST_ID: u64 = 1;

/// The agent's input, shared with whoever forwards Ctrl-C. `None` once closed.
pub type Shared<W> = Mutex<Option<W>>;

fn send<W: Write>(output: &Shared<W>, frame: &ClientFrame) -> Result<(), ProtoError> {
    match output.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        Some(w) => write_frame(w, frame),
        None => Ok(()),
    }
}

/// Asks the running request to stop, as Ctrl-C would at a terminal.
pub fn cancel<W: Write>(output: &Shared<W>) {
    let _ = send(output, &ClientFrame::Cancel { id: REQUEST_ID });
}

/// Closes the agent's input; it ends (interrupting what is left) when it sees that.
pub fn close<W: Write>(output: &Shared<W>) {
    *output.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Runs `request` on the agent behind `input`/`output`. Its standard output and
/// error are written to `out` and `err`; the result is the exit status the
/// command would have had locally.
pub fn run<R: BufRead, W: Write>(
    mut input: R,
    output: &Shared<W>,
    request: &Forward,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32> {
    send(output, &ClientFrame::Hello { version: PROTOCOL_VERSION, client: format!("yarc {}", env!("CARGO_PKG_VERSION")) })
        .map_err(lost)?;
    match read_frame::<_, AgentFrame>(&mut input).map_err(lost)? {
        Some(AgentFrame::Hello { version, .. }) => check_version(version).map_err(|e| {
            anyhow!("{e}; install the same yarc version inside WSL (see `yarc doctor`)")
        })?,
        Some(AgentFrame::Fatal { message }) => bail!("the agent refused the connection: {message}"),
        Some(other) => bail!("the agent started with an unexpected frame: {other:?}"),
        None => return Err(ended()),
    }

    send(
        output,
        &ClientFrame::Request { id: REQUEST_ID, globals: request.globals.clone(), command: request.command.clone() },
    )
    .map_err(lost)?;

    loop {
        match read_frame::<_, AgentFrame>(&mut input).map_err(lost)? {
            Some(AgentFrame::Event { id, event }) if id == REQUEST_ID => match event {
                Event::Stdout { text } => emit(out, &text),
                Event::Stderr { text } => emit(err, &text),
                Event::Exit { code } => return Ok(code),
                Event::Failed { message, code } => {
                    emit(err, &format!("error: {message}\n"));
                    return Ok(code);
                }
            },
            Some(AgentFrame::Event { .. }) => {} // not ours
            Some(AgentFrame::Fatal { message }) => bail!("the agent stopped: {message}"),
            Some(AgentFrame::Hello { .. }) => bail!("the agent sent a second hello"),
            None => return Err(ended()),
        }
    }
}

/// A closed terminal (broken pipe) must not stop the relay of the exit status.
fn emit(to: &mut dyn Write, text: &str) {
    let _ = to.write_all(text.as_bytes());
    let _ = to.flush();
}

fn lost(e: ProtoError) -> anyhow::Error {
    anyhow!("lost the connection to the agent: {e}")
}

fn ended() -> anyhow::Error {
    anyhow!("the agent ended before the command finished (is yarc installed in WSL? see `yarc doctor`)")
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Mutex;

    use yarc_proto::{Command, Globals};

    use super::*;

    fn request() -> Forward {
        Forward { globals: Globals::default(), command: Command::ImageLs }
    }

    fn frames(frames: &[AgentFrame]) -> Cursor<Vec<u8>> {
        let mut wire = Vec::new();
        for f in frames {
            write_frame(&mut wire, f).unwrap();
        }
        Cursor::new(wire)
    }

    fn hello() -> AgentFrame {
        AgentFrame::Hello { version: PROTOCOL_VERSION, agent: "test".into() }
    }

    fn event(event: Event) -> AgentFrame {
        AgentFrame::Event { id: REQUEST_ID, event }
    }

    struct Ran {
        result: Result<i32>,
        sent: Vec<ClientFrame>,
        out: String,
        err: String,
    }

    fn drive(script: &[AgentFrame]) -> Ran {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let sent = Mutex::new(Some(Vec::new()));
        let result = run(frames(script), &sent, &request(), &mut out, &mut err);
        let mut wire = Cursor::new(sent.lock().unwrap().take().unwrap());
        let mut frames = Vec::new();
        while let Some(f) = read_frame::<_, ClientFrame>(&mut wire).unwrap() {
            frames.push(f);
        }
        Ran { result, sent: frames, out: String::from_utf8(out).unwrap(), err: String::from_utf8(err).unwrap() }
    }

    #[test]
    fn output_is_relayed_and_the_exit_status_returned() {
        let r = drive(&[
            hello(),
            event(Event::Stdout { text: "a\n".into() }),
            event(Event::Stderr { text: "warn\n".into() }),
            event(Event::Stdout { text: "b\n".into() }),
            event(Event::Exit { code: 7 }),
        ]);
        assert_eq!(r.result.unwrap(), 7);
        assert_eq!((r.out.as_str(), r.err.as_str()), ("a\nb\n", "warn\n"));
        assert!(matches!(r.sent[0], ClientFrame::Hello { version: PROTOCOL_VERSION, .. }));
        assert!(matches!(&r.sent[1], ClientFrame::Request { id: REQUEST_ID, command: Command::ImageLs, .. }));
        assert_eq!(r.sent.len(), 2);
    }

    #[test]
    fn a_failed_request_prints_the_error_and_keeps_its_code() {
        let r = drive(&[hello(), event(Event::Failed { message: "boom".into(), code: 125 })]);
        assert_eq!(r.result.unwrap(), 125);
        assert_eq!(r.err, "error: boom\n");
    }

    #[test]
    fn events_of_other_requests_are_ignored() {
        let other = AgentFrame::Event { id: 99, event: Event::Stdout { text: "x".into() } };
        let r = drive(&[hello(), other, event(Event::Exit { code: 0 })]);
        assert_eq!((r.result.unwrap(), r.out.as_str()), (0, ""));
    }

    #[test]
    fn a_version_mismatch_is_explained() {
        let r = drive(&[AgentFrame::Hello { version: PROTOCOL_VERSION + 1, agent: "new".into() }]);
        let msg = r.result.unwrap_err().to_string();
        assert!(msg.contains("mismatch") && msg.contains("yarc doctor"), "{msg}");
        assert_eq!(r.sent.len(), 1, "no request is sent to an incompatible agent");
    }

    #[test]
    fn a_fatal_frame_or_a_missing_agent_is_an_error() {
        let r = drive(&[AgentFrame::Fatal { message: "no".into() }]);
        assert!(r.result.unwrap_err().to_string().contains("refused"));
        let r = drive(&[]);
        assert!(r.result.unwrap_err().to_string().contains("yarc doctor"));
        let r = drive(&[hello(), event(Event::Stdout { text: "partial".into() })]);
        assert!(r.result.unwrap_err().to_string().contains("ended"));
        assert_eq!(r.out, "partial", "what arrived is still shown");
    }

    #[test]
    fn cancel_sends_a_frame_for_the_request_and_close_ends_the_input() {
        let output = Mutex::new(Some(Vec::new()));
        cancel(&output);
        let bytes = output.lock().unwrap().clone().unwrap();
        let frame: ClientFrame = read_frame(&mut Cursor::new(bytes)).unwrap().unwrap();
        assert_eq!(frame, ClientFrame::Cancel { id: REQUEST_ID });

        close(&output);
        assert!(output.lock().unwrap().is_none(), "the writer is dropped, which closes a pipe");
        cancel(&output); // after close: nothing to write to, and no panic
    }

    #[test]
    fn a_closed_terminal_does_not_lose_the_exit_status() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sent = Mutex::new(Some(Vec::new()));
        let script = frames(&[hello(), event(Event::Stdout { text: "x".into() }), event(Event::Exit { code: 3 })]);
        assert_eq!(run(script, &sent, &request(), &mut Closed, &mut Closed).unwrap(), 3);
    }
}
