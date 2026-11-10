use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

use crate::ConfigError;

#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub services: BTreeMap<String, Service>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Service {
    pub image: String,
    /// Replaces the image's default command (like Docker's `CMD`); empty keeps it.
    pub cmd: Vec<String>,
    /// Replaces the image's `ENTRYPOINT`; `Some(vec![])` clears it, `None` keeps it.
    pub entrypoint: Option<Vec<String>>,
    /// Replaces the image's working directory; `None` keeps it.
    pub working_dir: Option<String>,
    /// Extra name the service answers to inside the project network (compose `container_name`).
    pub container_name: Option<String>,
    /// Replaces the image's `USER` (name or `uid[:gid]`); `None` keeps it.
    pub user: Option<String>,
    pub env: BTreeMap<String, String>,
    pub ports: Vec<PortMapping>,
    pub volumes: Vec<VolumeMount>,
    pub health: Option<Health>,
    pub needs: BTreeMap<String, Condition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Condition {
    Started,
    Healthy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortMapping {
    pub host: u16,
    pub container: u16,
    pub protocol: Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeSource {
    /// Implicit named volume managed by yarc.
    Named(String),
    /// Host path (absolute, or starting with `.` / `~`).
    Host(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeMount {
    pub source: VolumeSource,
    pub target: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub cmd: String,
    pub interval: Duration,
}

pub const DEFAULT_HEALTH_INTERVAL: Duration = Duration::from_secs(30);

// ---- raw (serde) layer ------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawProject {
    #[serde(default)]
    services: BTreeMap<String, RawService>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawService {
    image: String,
    #[serde(default)]
    cmd: Vec<String>,
    user: Option<RawUser>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    ports: Vec<String>,
    #[serde(default)]
    volumes: Vec<String>,
    health: Option<RawHealth>,
    #[serde(default)]
    needs: BTreeMap<String, Condition>,
}

/// `user: root`, `user: "1000:1000"` or a bare `user: 0`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawUser {
    Name(String),
    Id(u32),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHealth {
    cmd: String,
    interval: Option<String>,
}

impl RawProject {
    pub(crate) fn into_project(self) -> Result<Project, ConfigError> {
        let mut services = BTreeMap::new();
        for (name, raw) in self.services {
            let svc = raw.into_service(&name)?;
            services.insert(name, svc);
        }
        Ok(Project { services })
    }
}

impl RawService {
    fn into_service(self, name: &str) -> Result<Service, ConfigError> {
        if self.image.trim().is_empty() {
            return Err(ConfigError::EmptyImage(name.into()));
        }
        let user = self.user.map(|u| match u {
            RawUser::Name(s) => s.trim().to_string(),
            RawUser::Id(n) => n.to_string(),
        });
        if user.as_deref() == Some("") {
            return Err(ConfigError::EmptyUser(name.into()));
        }
        let ports = self
            .ports
            .iter()
            .map(|p| parse_port(name, p))
            .collect::<Result<_, _>>()?;
        let volumes = self
            .volumes
            .iter()
            .map(|v| parse_volume(name, v))
            .collect::<Result<_, _>>()?;
        let health = self
            .health
            .map(|h| -> Result<Health, ConfigError> {
                let interval = match h.interval {
                    Some(s) => parse_duration(&s).ok_or_else(|| ConfigError::InvalidDuration {
                        service: name.into(),
                        value: s,
                    })?,
                    None => DEFAULT_HEALTH_INTERVAL,
                };
                Ok(Health { cmd: h.cmd, interval })
            })
            .transpose()?;
        Ok(Service {
            image: self.image,
            cmd: self.cmd,
            entrypoint: None,
            working_dir: None,
            container_name: None,
            user,
            env: self.env,
            ports,
            volumes,
            health,
            needs: self.needs,
        })
    }
}

pub(crate) fn parse_port(service: &str, value: &str) -> Result<PortMapping, ConfigError> {
    let err = |reason: &str| ConfigError::InvalidPort {
        service: service.into(),
        value: value.into(),
        reason: reason.into(),
    };
    let (ports, protocol) = match value.split_once('/') {
        None => (value, Protocol::Tcp),
        Some((p, "tcp")) => (p, Protocol::Tcp),
        Some((p, "udp")) => (p, Protocol::Udp),
        Some(_) => return Err(err("protocol must be tcp or udp")),
    };
    let (host, container) = ports
        .split_once(':')
        .ok_or_else(|| err("expected HOST:CONTAINER"))?;
    let num = |s: &str| match s.parse::<u16>() {
        Ok(0) | Err(_) => Err(err("port must be 1-65535")),
        Ok(n) => Ok(n),
    };
    Ok(PortMapping { host: num(host)?, container: num(container)?, protocol })
}

pub(crate) fn parse_volume(service: &str, value: &str) -> Result<VolumeMount, ConfigError> {
    let err = |reason: &str| ConfigError::InvalidVolume {
        service: service.into(),
        value: value.into(),
        reason: reason.into(),
    };
    let parts: Vec<&str> = value.split(':').collect();
    let (source, target, read_only) = match parts.as_slice() {
        [s, t] => (*s, *t, false),
        [s, t, "ro"] => (*s, *t, true),
        [_, _, _] => return Err(err("mode must be 'ro'")),
        _ => return Err(err("expected SOURCE:TARGET[:ro]")),
    };
    if source.is_empty() {
        return Err(err("source must not be empty"));
    }
    if !target.starts_with('/') {
        return Err(err("target must be an absolute path"));
    }
    let source = if source.starts_with('/') || source.starts_with('.') || source.starts_with('~') {
        VolumeSource::Host(source.into())
    } else {
        VolumeSource::Named(source.into())
    };
    Ok(VolumeMount { source, target: target.into(), read_only })
}

/// Parses `500ms`, `5s`, `2m`, `1h` and compound forms such as `1m30s`.
pub(crate) fn parse_duration(s: &str) -> Option<Duration> {
    let mut rest = s.trim();
    if rest.is_empty() {
        return None;
    }
    let mut total = Duration::ZERO;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        let n: u64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let (unit, len) = if rest.starts_with("ms") {
            (Duration::from_millis(n), 2)
        } else if rest.starts_with('s') {
            (Duration::from_secs(n), 1)
        } else if rest.starts_with('m') {
            (Duration::from_secs(n.checked_mul(60)?), 1)
        } else if rest.starts_with('h') {
            (Duration::from_secs(n.checked_mul(3600)?), 1)
        } else {
            return None;
        };
        total = total.checked_add(unit)?;
        rest = &rest[len..];
    }
    Some(total)
}
