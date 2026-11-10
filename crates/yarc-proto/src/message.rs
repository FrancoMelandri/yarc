//! The messages. Frames are internally tagged (`"type"`), field and variant
//! names are `snake_case`; unknown fields are ignored and optional ones
//! default, so either side can grow without breaking the other within a
//! protocol version.
//!
//! Every path in a request is a path *as the agent sees it* (Linux): a client
//! on another OS translates the typed path fields before sending.

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to the frames below.
pub const PROTOCOL_VERSION: u32 = 1;

/// Request identifier chosen by the client; every event answers one.
pub type RequestId = u64;

/// Client -> agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// First frame; the agent answers with its own `Hello`.
    Hello { version: u32, client: String },
    /// Run one command. Several may be in flight; their events are interleaved.
    Request { id: RequestId, globals: Globals, command: Command },
    /// Ask a running request to stop (Ctrl-C): the agent behaves as if the
    /// command had received SIGINT.
    Cancel { id: RequestId },
}

/// Agent -> client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentFrame {
    Hello { version: u32, agent: String },
    Event { id: RequestId, event: Event },
    /// The agent cannot continue (bad frame, version mismatch); it exits after this.
    Fatal { message: String },
}

/// What a request produces, ending with exactly one `Exit` or `Failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A chunk of the command's standard output, as text.
    Stdout { text: String },
    /// A chunk of the command's standard error, as text.
    Stderr { text: String },
    /// The command finished; `code` is what the CLI would exit with.
    Exit { code: i32 },
    /// The command failed; `message` is what the CLI would print after `error: `.
    Failed { message: String, code: i32 },
}

/// The global options of the CLI.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Globals {
    /// Store location; `None`: the agent's default.
    #[serde(default)]
    pub root: Option<String>,
    /// Project file (path).
    #[serde(default)]
    pub file: Option<String>,
    /// Project name.
    #[serde(default)]
    pub project: Option<String>,
    /// Active compose profiles (`--profile`, repeatable).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<String>,
}

/// Everything `create` and `run` need.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSpec {
    pub image: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    /// `host` or `none`.
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    // images
    Pull { image: String, #[serde(default)] jobs: Option<u16>, #[serde(default)] platform: Option<String> },
    /// `file` is a path.
    Load { file: String },
    ImageLs,
    ImageRm { images: Vec<String> },
    /// `dir` is a path.
    ImageUnpack { image: String, dir: String },
    // containers
    Create(CreateSpec),
    Run {
        #[serde(flatten)]
        spec: CreateSpec,
        #[serde(default)]
        rm: bool,
        #[serde(default)]
        detach: bool,
        #[serde(default)]
        debug: bool,
    },
    Ps { #[serde(default)] all: bool },
    ContainerLs,
    Start { id: String, #[serde(default)] detach: bool, #[serde(default)] debug: bool },
    Exec {
        id: String,
        #[serde(default)]
        env: Vec<String>,
        #[serde(default)]
        workdir: Option<String>,
        cmd: Vec<String>,
    },
    Stop { ids: Vec<String>, #[serde(default)] time: Option<u64> },
    Kill { ids: Vec<String>, #[serde(default)] signal: Option<String> },
    Wait { ids: Vec<String> },
    Logs {
        id: String,
        #[serde(default)]
        tail: Option<usize>,
        #[serde(default)]
        timestamps: bool,
        #[serde(default)]
        follow: bool,
    },
    Rm { ids: Vec<String>, #[serde(default)] force: bool },
    // projects
    Up { #[serde(default)] detach: bool },
    Down { #[serde(default)] time: Option<u64>, #[serde(default)] volumes: bool },
    ProjectPs,
    ProjectLogs {
        #[serde(default)]
        services: Vec<String>,
        #[serde(default)]
        tail: Option<usize>,
        #[serde(default)]
        timestamps: bool,
        #[serde(default)]
        follow: bool,
    },
    ProjectStop { #[serde(default)] time: Option<u64> },
    ProjectConfig,
}
