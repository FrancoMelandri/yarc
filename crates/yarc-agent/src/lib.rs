//! The yarc agent: serves `yarc-proto` requests on a pair of byte streams.
//!
//! Every request runs as a child process of the same `yarc` binary, with the
//! arguments the equivalent command line would have (`argv`). Its stdout and
//! stderr become `Stdout`/`Stderr` events, its exit status the final `Exit`
//! event, and a `Cancel` frame is a SIGINT to it, exactly like Ctrl-C at a
//! terminal. So the agent behaves as the CLI does, whatever the command
//! prints and however the runtime does it, and requests cannot disturb each
//! other. There is no state between requests and no daemon: the agent lives as
//! long as its input stays open.
//!
//! Commands get no stdin yet (no `exec` with a terminal).

mod argv;
#[cfg(unix)]
mod server;
#[cfg(unix)]
mod utf8;

pub use argv::{to_args, ArgError};
#[cfg(unix)]
pub use server::serve;
