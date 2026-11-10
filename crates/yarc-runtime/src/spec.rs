//! What to run: the container spec and how it is derived from image defaults
//! plus user overrides (pure, no I/O).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Result, RuntimeError};
use yarc_config::RestartPolicy;

/// Used when neither the image nor the user sets `PATH` (same as Docker).
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// Share the host's network namespace.
    #[default]
    Host,
    /// New network namespace with only loopback.
    None,
    /// Join the project's shared network namespace (see `network.rs`): loopback
    /// only, shared with the other services of the project.
    Project,
}

impl FromStr for Network {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "host" => Ok(Network::Host),
            "none" => Ok(Network::None),
            other => Err(format!("unknown network '{other}', expected host or none")),
        }
    }
}

/// Process-related defaults taken from the image config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageDefaults {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    /// `KEY=VALUE` entries.
    pub env: Vec<String>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
}

/// What the user asked for on the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// `Some(vec![])` clears the image's entrypoint.
    pub entrypoint: Option<Vec<String>>,
    /// Replaces the image's Cmd when not empty.
    pub cmd: Vec<String>,
    /// `KEY=VALUE` entries; they win over the image's.
    pub env: Vec<String>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_dir: String,
    pub user: Option<String>,
}

/// Docker semantics: an entrypoint override discards the image's Cmd; otherwise
/// user-supplied args replace Cmd but keep the image's Entrypoint.
pub fn resolve_process(defaults: &ImageDefaults, ov: &Overrides) -> Result<Process> {
    let (entrypoint, cmd) = match &ov.entrypoint {
        Some(ep) => (ep.clone(), ov.cmd.clone()),
        None if !ov.cmd.is_empty() => (defaults.entrypoint.clone(), ov.cmd.clone()),
        None => (defaults.entrypoint.clone(), defaults.cmd.clone()),
    };
    let args: Vec<String> = entrypoint.into_iter().chain(cmd).collect();
    if args.is_empty() {
        return Err(RuntimeError::NoCommand);
    }

    let mut env: Vec<(String, String)> = Vec::new();
    // Image entries are trusted-ish: a malformed one is skipped, not fatal.
    let image_env = defaults.env.iter().filter_map(|e| split_env(e));
    let user_env = ov.env.iter().map(|e| split_env(e).ok_or_else(|| RuntimeError::InvalidEnv(e.clone())));
    for kv in image_env.map(Ok).chain(user_env) {
        let (k, v) = kv?;
        match env.iter_mut().find(|(ek, _)| *ek == k) {
            Some(slot) => slot.1 = v,
            None => env.push((k, v)),
        }
    }
    if !env.iter().any(|(k, _)| k == "PATH") {
        env.push(("PATH".into(), DEFAULT_PATH.into()));
    }

    let working_dir = ov
        .working_dir
        .clone()
        .or_else(|| defaults.working_dir.clone().filter(|w| !w.is_empty()))
        .unwrap_or_else(|| "/".into());
    if !working_dir.starts_with('/') {
        return Err(RuntimeError::RelativeWorkDir(working_dir));
    }

    let user = ov.user.clone().or_else(|| defaults.user.clone().filter(|u| !u.is_empty()));
    Ok(Process { args, env, working_dir, user })
}

fn split_env(entry: &str) -> Option<(String, String)> {
    let (k, v) = entry.split_once('=')?;
    (!k.is_empty()).then(|| (k.to_string(), v.to_string()))
}

/// A host directory or file mounted into the container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSpec {
    /// Absolute path on the host; must exist when the container starts.
    pub source: PathBuf,
    /// Absolute path inside the container.
    pub target: String,
    pub read_only: bool,
}

pub const DEFAULT_HEALTH_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_HEALTH_RETRIES: u32 = 3;

/// A periodic check run *inside* the container; exit status 0 means healthy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSpec {
    pub cmd: Vec<String>,
    pub interval_ms: u64,
    /// A check running longer than this counts as a failure.
    pub timeout_ms: u64,
    /// Consecutive failures before the container is `unhealthy`.
    pub retries: u32,
}

impl HealthSpec {
    /// A shell command line (`sh -c`), with Docker's default timeout and retries.
    pub fn shell(cmd: &str, interval: std::time::Duration) -> Self {
        HealthSpec {
            cmd: vec!["sh".into(), "-c".into(), cmd.into()],
            interval_ms: interval.as_millis() as u64,
            timeout_ms: DEFAULT_HEALTH_TIMEOUT_MS,
            retries: DEFAULT_HEALTH_RETRIES,
        }
    }
}

/// Everything needed to start one container; persisted as `config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerSpec {
    pub id: String,
    /// Image reference it was created from (informational).
    pub image: String,
    pub rootfs: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_dir: String,
    pub user: Option<String>,
    pub hostname: String,
    pub network: Network,
    /// Free-form metadata, e.g. which project and service a container belongs to.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub health: Option<HealthSpec>,
    /// With `Network::Project`: the project's network directory (holder pid,
    /// lock). Resolved when the container starts, so a restarted holder works.
    #[serde(default)]
    pub project_net: Option<PathBuf>,
    /// A hosts file shared by the project's containers, bind-mounted over
    /// `/etc/hosts` (maps service names to the shared loopback).
    #[serde(default)]
    pub hosts_file: Option<PathBuf>,
    #[serde(default)]
    pub volumes: Vec<VolumeSpec>,
    /// What the supervisor does when the container exits (detached containers only).
    #[serde(default, skip_serializing_if = "RestartPolicy::is_no")]
    pub restart: RestartPolicy,
}

impl ContainerSpec {
    pub fn with_restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }

    pub fn with_volumes(mut self, volumes: Vec<VolumeSpec>) -> Self {
        self.volumes = volumes;
        self
    }

    /// Puts the container in a project's shared network namespace.
    pub fn in_project_network(mut self, project_net: PathBuf, hosts_file: PathBuf) -> Self {
        self.network = Network::Project;
        self.project_net = Some(project_net);
        self.hosts_file = Some(hosts_file);
        self
    }

    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = labels;
        self
    }

    pub fn with_health(mut self, health: Option<HealthSpec>) -> Self {
        self.health = health;
        self
    }

    pub fn new(id: &str, image: &str, rootfs: PathBuf, process: Process, network: Network) -> Self {
        ContainerSpec {
            id: id.to_string(),
            image: image.to_string(),
            rootfs,
            args: process.args,
            env: process.env,
            working_dir: process.working_dir,
            user: process.user,
            hostname: id.chars().take(12).collect(),
            network,
            labels: BTreeMap::new(),
            health: None,
            project_net: None,
            hosts_file: None,
            volumes: Vec::new(),
            restart: RestartPolicy::No,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn nginx() -> ImageDefaults {
        ImageDefaults {
            entrypoint: s(&["/docker-entrypoint.sh"]),
            cmd: s(&["nginx", "-g", "daemon off;"]),
            env: s(&["PATH=/usr/bin", "NGINX_VERSION=1.27"]),
            working_dir: None,
            user: None,
        }
    }

    #[test]
    fn the_restart_policy_is_stored_only_when_set() {
        let p = Process { args: s(&["x"]), env: vec![], working_dir: "/".into(), user: None };
        let plain = ContainerSpec::new("c", "img", "/r".into(), p, Network::Host);
        assert_eq!(plain.restart, RestartPolicy::No);
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("restart"), "{json}");
        // a config.json written before restart policies existed still loads
        assert_eq!(serde_json::from_str::<ContainerSpec>(&json).unwrap().restart, RestartPolicy::No);

        let always = plain.with_restart(RestartPolicy::OnFailure { max: Some(3) });
        let json = serde_json::to_string(&always).unwrap();
        assert!(json.contains("\"restart\":\"on-failure:3\""), "{json}");
        assert_eq!(serde_json::from_str::<ContainerSpec>(&json).unwrap(), always);
    }

    #[test]
    fn image_defaults_are_used() {
        let p = resolve_process(&nginx(), &Overrides::default()).unwrap();
        assert_eq!(p.args, s(&["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"]));
        assert_eq!(p.working_dir, "/");
        assert_eq!(p.user, None);
    }

    #[test]
    fn cli_args_replace_cmd_but_keep_entrypoint() {
        let ov = Overrides { cmd: s(&["echo", "hi"]), ..Default::default() };
        let p = resolve_process(&nginx(), &ov).unwrap();
        assert_eq!(p.args, s(&["/docker-entrypoint.sh", "echo", "hi"]));
    }

    #[test]
    fn entrypoint_override_drops_image_cmd() {
        let ov = Overrides { entrypoint: Some(s(&["/bin/sh"])), ..Default::default() };
        assert_eq!(resolve_process(&nginx(), &ov).unwrap().args, s(&["/bin/sh"]));
        let ov = Overrides { entrypoint: Some(s(&["/bin/sh"])), cmd: s(&["-c", "id"]), ..Default::default() };
        assert_eq!(resolve_process(&nginx(), &ov).unwrap().args, s(&["/bin/sh", "-c", "id"]));
    }

    #[test]
    fn cleared_entrypoint_runs_cli_args_alone() {
        let ov = Overrides { entrypoint: Some(vec![]), cmd: s(&["ls"]), ..Default::default() };
        assert_eq!(resolve_process(&nginx(), &ov).unwrap().args, s(&["ls"]));
    }

    #[test]
    fn nothing_to_run_is_an_error() {
        assert!(matches!(
            resolve_process(&ImageDefaults::default(), &Overrides::default()),
            Err(RuntimeError::NoCommand)
        ));
        let ov = Overrides { entrypoint: Some(vec![]), ..Default::default() };
        assert!(matches!(resolve_process(&nginx(), &ov), Err(RuntimeError::NoCommand)));
    }

    #[test]
    fn env_merges_with_user_values_winning() {
        let ov = Overrides { env: s(&["NGINX_VERSION=2", "EXTRA=a=b"]), cmd: s(&["x"]), ..Default::default() };
        let p = resolve_process(&nginx(), &ov).unwrap();
        assert_eq!(
            p.env,
            vec![
                ("PATH".into(), "/usr/bin".into()),
                ("NGINX_VERSION".into(), "2".into()),
                ("EXTRA".into(), "a=b".into()),
            ]
        );
    }

    #[test]
    fn default_path_is_added_only_when_missing() {
        let d = ImageDefaults { cmd: s(&["x"]), ..Default::default() };
        let p = resolve_process(&d, &Overrides::default()).unwrap();
        assert_eq!(p.env, vec![("PATH".into(), DEFAULT_PATH.into())]);
    }

    #[test]
    fn malformed_user_env_is_rejected_but_image_env_is_skipped() {
        let d = ImageDefaults { cmd: s(&["x"]), env: s(&["BROKEN", "A=1"]), ..Default::default() };
        let p = resolve_process(&d, &Overrides::default()).unwrap();
        assert_eq!(p.env[0], ("A".into(), "1".into()));
        for bad in ["NOEQUALS", "=v"] {
            let ov = Overrides { env: s(&[bad]), ..Default::default() };
            assert!(matches!(resolve_process(&d, &ov), Err(RuntimeError::InvalidEnv(_))), "{bad}");
        }
    }

    #[test]
    fn working_dir_and_user_precedence() {
        let d = ImageDefaults {
            cmd: s(&["x"]),
            working_dir: Some("/app".into()),
            user: Some("node".into()),
            ..Default::default()
        };
        let p = resolve_process(&d, &Overrides::default()).unwrap();
        assert_eq!((p.working_dir.as_str(), p.user.as_deref()), ("/app", Some("node")));
        let ov = Overrides { working_dir: Some("/tmp".into()), user: Some("0".into()), ..Default::default() };
        let p = resolve_process(&d, &ov).unwrap();
        assert_eq!((p.working_dir.as_str(), p.user.as_deref()), ("/tmp", Some("0")));
        // empty strings in image configs mean "unset"
        let d = ImageDefaults {
            cmd: s(&["x"]),
            working_dir: Some(String::new()),
            user: Some(String::new()),
            ..Default::default()
        };
        let p = resolve_process(&d, &Overrides::default()).unwrap();
        assert_eq!((p.working_dir.as_str(), p.user), ("/", None));
    }

    #[test]
    fn relative_working_dir_is_rejected() {
        let ov = Overrides { working_dir: Some("app".into()), cmd: s(&["x"]), ..Default::default() };
        assert!(matches!(resolve_process(&ImageDefaults::default(), &ov), Err(RuntimeError::RelativeWorkDir(_))));
    }

    #[test]
    fn network_parsing() {
        assert_eq!("host".parse(), Ok(Network::Host));
        assert_eq!("none".parse(), Ok(Network::None));
        assert!("bridge".parse::<Network>().is_err());
    }

    #[test]
    fn labels_roundtrip_and_old_configs_without_labels_still_load() {
        let p = Process { args: s(&["x"]), env: vec![], working_dir: "/".into(), user: None };
        let labels = BTreeMap::from([("yarc.project".to_string(), "demo".to_string())]);
        let spec = ContainerSpec::new("c", "alpine", "/r".into(), p, Network::Host).with_labels(labels.clone());
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<ContainerSpec>(&json).unwrap().labels, labels);

        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value.as_object_mut().unwrap().remove("labels");
        let old: ContainerSpec = serde_json::from_value(value).unwrap();
        assert!(old.labels.is_empty());
    }

    #[test]
    fn health_spec_roundtrips_and_is_optional_in_old_configs() {
        let p = Process { args: s(&["x"]), env: vec![], working_dir: "/".into(), user: None };
        let health = HealthSpec::shell("pg_isready -q", std::time::Duration::from_millis(1500));
        assert_eq!(health.cmd, ["sh", "-c", "pg_isready -q"]);
        assert_eq!((health.interval_ms, health.timeout_ms, health.retries), (1500, 30_000, 3));
        let spec = ContainerSpec::new("c", "img", "/r".into(), p, Network::Host).with_health(Some(health.clone()));
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<ContainerSpec>(&json).unwrap().health, Some(health));

        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value.as_object_mut().unwrap().remove("health");
        assert_eq!(serde_json::from_value::<ContainerSpec>(value).unwrap().health, None);
    }

    #[test]
    fn volumes_roundtrip_and_are_optional_in_old_configs() {
        let p = Process { args: s(&["x"]), env: vec![], working_dir: "/".into(), user: None };
        let vol = VolumeSpec { source: "/data".into(), target: "/var/lib/x".into(), read_only: true };
        let spec = ContainerSpec::new("c", "img", "/r".into(), p, Network::Host).with_volumes(vec![vol.clone()]);
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<ContainerSpec>(&json).unwrap().volumes, vec![vol]);
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value.as_object_mut().unwrap().remove("volumes");
        assert!(serde_json::from_value::<ContainerSpec>(value).unwrap().volumes.is_empty());
    }

    #[test]
    fn spec_uses_short_id_as_hostname() {
        let p = Process { args: s(&["x"]), env: vec![], working_dir: "/".into(), user: None };
        let spec = ContainerSpec::new("0123456789abcdef", "alpine", "/r".into(), p, Network::Host);
        assert_eq!(spec.hostname, "0123456789ab");
    }
}
