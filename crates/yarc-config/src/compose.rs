//! Reader for Docker Compose files, filling the same [`Project`] model as `yarc.yaml`.
//!
//! Supported per service: `image`, `command`, `entrypoint`, `environment` (list or map),
//! `ports` (short form), `volumes` (short form), `depends_on` (list or map with
//! `condition`), `user`, `working_dir`, `healthcheck` (`test`, `interval`) and
//! `container_name`. Top-level `volumes` may only declare names. `x-...` keys and the
//! obsolete `version` are ignored; every other key is an error, so nothing is silently
//! dropped.
//!
//! `${...}` interpolation runs on the string values of the parsed YAML (as Docker does),
//! never on comments or keys.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_yaml_ng::Value;

use crate::interpolate::interpolate;
use crate::model::{parse_duration, parse_port, parse_volume, DEFAULT_HEALTH_INTERVAL};
use crate::{validate, Condition, ConfigError, Health, PortMapping, Project, Service, VolumeSource};

#[derive(Debug, Deserialize)]
struct RawCompose {
    #[serde(default)]
    services: BTreeMap<String, RawService>,
    #[serde(default)]
    volumes: Option<BTreeMap<String, Value>>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct RawService {
    image: Option<String>,
    command: Option<Words>,
    entrypoint: Option<Words>,
    environment: Option<Value>,
    #[serde(default)]
    ports: Vec<Value>,
    #[serde(default)]
    volumes: Vec<Value>,
    depends_on: Option<Value>,
    user: Option<Value>,
    working_dir: Option<String>,
    healthcheck: Option<RawHealthcheck>,
    container_name: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct RawHealthcheck {
    test: Option<Words>,
    interval: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

/// A command given as one string (split like a shell) or as a list.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Words {
    Line(String),
    List(Vec<Value>),
}

/// Parse a compose document. `lookup` resolves `${VAR}` and `environment:` pass-through
/// entries (normally the process environment layered over a `.env` file).
pub fn parse_compose<F>(input: &str, lookup: &F) -> Result<Project, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let mut doc: Value = serde_yaml_ng::from_str(input).map_err(|e| ConfigError::Yaml(e.to_string()))?;
    doc.apply_merge().map_err(|e| ConfigError::Yaml(e.to_string()))?;
    interpolate_values(&mut doc, "", lookup)?;
    let raw: RawCompose = serde_yaml_ng::from_value(doc).map_err(|e| ConfigError::Yaml(e.to_string()))?;

    for key in raw.extra.keys() {
        if key != "version" && !key.starts_with("x-") {
            return Err(ConfigError::UnsupportedKey { scope: "top level".into(), key: key.clone() });
        }
    }
    let mut declared_volumes = Vec::new();
    for (name, def) in raw.volumes.unwrap_or_default() {
        let empty = match &def {
            Value::Null => true,
            Value::Mapping(m) => m.is_empty(),
            _ => false,
        };
        if !empty {
            return Err(ConfigError::Field {
                service: String::new(),
                field: format!("volumes.{name}"),
                reason: "volume options are not supported (declare the name only)".into(),
            });
        }
        declared_volumes.push(name);
    }

    let mut services = BTreeMap::new();
    for (name, svc) in raw.services {
        services.insert(name.clone(), convert_service(&name, svc, &declared_volumes, lookup)?);
    }
    let project = Project { services };
    validate::validate(&project)?;
    Ok(project)
}

fn interpolate_values<F>(v: &mut Value, path: &str, lookup: &F) -> Result<(), ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    match v {
        Value::String(s) => {
            *s = interpolate(s, lookup).map_err(|e| ConfigError::Interpolation {
                path: path.to_string(),
                reason: e.to_string(),
            })?;
        }
        Value::Sequence(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                interpolate_values(item, &format!("{path}[{i}]"), lookup)?;
            }
        }
        Value::Mapping(map) => {
            for (k, item) in map.iter_mut() {
                let key = k.as_str().unwrap_or("?");
                let child = if path.is_empty() { key.to_string() } else { format!("{path}.{key}") };
                interpolate_values(item, &child, lookup)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn field_err(service: &str, field: &str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Field { service: service.into(), field: field.into(), reason: reason.into() }
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn words(service: &str, field: &str, w: Words) -> Result<Vec<String>, ConfigError> {
    match w {
        Words::Line(s) => split_words(&s).ok_or_else(|| field_err(service, field, "unbalanced quotes")),
        Words::List(items) => items
            .iter()
            .map(|v| scalar(v).ok_or_else(|| field_err(service, field, "list items must be scalars")))
            .collect(),
    }
}

/// Splits a command line like a POSIX shell would for plain words: whitespace separates,
/// `'...'` is literal, `"..."` honours backslash escapes, and a backslash escapes the next char.
fn split_words(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => cur.push(c),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            e @ ('"' | '\\' | '$' | '`') => cur.push(e),
                            other => {
                                cur.push('\\');
                                cur.push(other);
                            }
                        },
                        c => cur.push(c),
                    }
                }
            }
            '\\' => {
                started = true;
                cur.push(chars.next()?);
            }
            c => {
                started = true;
                cur.push(c);
            }
        }
    }
    if started {
        out.push(cur);
    }
    Some(out)
}

/// Quotes one argument for `sh -c`.
fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c));
    if safe { arg.to_string() } else { format!("'{}'", arg.replace('\'', r"'\''")) }
}

fn convert_service<F>(
    name: &str,
    raw: RawService,
    declared_volumes: &[String],
    lookup: &F,
) -> Result<Service, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    for key in raw.extra.keys() {
        if !key.starts_with("x-") {
            return Err(ConfigError::UnsupportedKey { scope: format!("service '{name}'"), key: key.clone() });
        }
    }
    let image = raw.image.ok_or_else(|| field_err(name, "image", "is required"))?;
    if image.trim().is_empty() {
        return Err(ConfigError::EmptyImage(name.into()));
    }

    let cmd = raw.command.map(|w| words(name, "command", w)).transpose()?.unwrap_or_default();
    let entrypoint = raw.entrypoint.map(|w| words(name, "entrypoint", w)).transpose()?;

    let user = match raw.user {
        None => None,
        Some(v) => {
            let u = scalar(&v).map(|s| s.trim().to_string()).ok_or_else(|| field_err(name, "user", "must be a string or number"))?;
            if u.is_empty() {
                return Err(ConfigError::EmptyUser(name.into()));
            }
            Some(u)
        }
    };

    if let Some(dir) = &raw.working_dir {
        if !dir.starts_with('/') {
            return Err(field_err(name, "working_dir", "must be an absolute path"));
        }
    }

    if let Some(cn) = &raw.container_name {
        let ok = !cn.is_empty() && cn.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
        if !ok {
            return Err(field_err(name, "container_name", "may only contain letters, digits, '_', '.' and '-'"));
        }
    }

    let env = convert_environment(name, raw.environment, lookup)?;
    let ports = raw.ports.iter().map(|p| convert_port(name, p)).collect::<Result<Vec<_>, _>>()?;
    let volumes = convert_volumes(name, &raw.volumes, declared_volumes)?;
    let needs = convert_depends_on(name, raw.depends_on)?;
    let health = raw.healthcheck.map(|h| convert_health(name, h)).transpose()?.flatten();

    Ok(Service {
        image,
        cmd,
        entrypoint,
        working_dir: raw.working_dir,
        container_name: raw.container_name,
        user,
        env,
        ports,
        volumes,
        health,
        needs,
    })
}

fn convert_environment<F>(
    name: &str,
    env: Option<Value>,
    lookup: &F,
) -> Result<BTreeMap<String, String>, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let mut out = BTreeMap::new();
    let check_key = |k: &str| {
        if k.is_empty() || k.contains('=') {
            Err(field_err(name, "environment", "variable names must be non-empty and contain no '='"))
        } else {
            Ok(())
        }
    };
    match env {
        None | Some(Value::Null) => {}
        Some(Value::Sequence(items)) => {
            for item in items {
                let s = scalar(&item).ok_or_else(|| field_err(name, "environment", "entries must be KEY=value strings"))?;
                match s.split_once('=') {
                    Some((k, v)) => {
                        check_key(k)?;
                        out.insert(k.to_string(), v.to_string());
                    }
                    None => {
                        check_key(&s)?;
                        if let Some(v) = lookup(&s) {
                            out.insert(s, v);
                        }
                    }
                }
            }
        }
        Some(Value::Mapping(map)) => {
            for (k, v) in map {
                let k = k.as_str().ok_or_else(|| field_err(name, "environment", "names must be strings"))?.to_string();
                check_key(&k)?;
                match v {
                    Value::Null => {
                        if let Some(v) = lookup(&k) {
                            out.insert(k, v);
                        }
                    }
                    v => {
                        let v = scalar(&v).ok_or_else(|| field_err(name, "environment", format!("value of {k} must be a scalar")))?;
                        out.insert(k, v);
                    }
                }
            }
        }
        Some(_) => return Err(field_err(name, "environment", "must be a list or a map")),
    }
    Ok(out)
}

fn convert_port(name: &str, v: &Value) -> Result<PortMapping, ConfigError> {
    let Value::String(s) = v else {
        let reason = if v.is_mapping() {
            "the long syntax is not supported; use \"HOST:CONTAINER\""
        } else {
            "expected \"HOST:CONTAINER\" (a bare container port needs a random host port, which is not supported)"
        };
        return Err(field_err(name, "ports", reason));
    };
    let (addrs, proto) = match s.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (s.as_str(), String::new()),
    };
    let parts: Vec<&str> = addrs.split(':').collect();
    let hostport = match parts.as_slice() {
        [_] => {
            return Err(field_err(name, "ports", format!("'{s}': a bare container port needs a random host port, which is not supported")))
        }
        [h, c] => format!("{h}:{c}{proto}"),
        [ip, h, c] => {
            if *ip != "127.0.0.1" && *ip != "localhost" {
                return Err(field_err(name, "ports", format!("'{s}': yarc publishes on 127.0.0.1 only")));
            }
            format!("{h}:{c}{proto}")
        }
        _ => return Err(field_err(name, "ports", format!("'{s}': expected [127.0.0.1:]HOST:CONTAINER[/tcp|udp]"))),
    };
    parse_port(name, &hostport)
}

fn convert_volumes(
    name: &str,
    volumes: &[Value],
    declared: &[String],
) -> Result<Vec<crate::VolumeMount>, ConfigError> {
    let mut out = Vec::new();
    for v in volumes {
        let Value::String(s) = v else {
            return Err(field_err(name, "volumes", "only the short \"SOURCE:TARGET[:ro]\" syntax is supported"));
        };
        // `:rw` is compose's explicit default; yarc.yaml has no spelling for it
        let mount = parse_volume(name, s.strip_suffix(":rw").unwrap_or(s))?;
        if let VolumeSource::Named(n) = &mount.source {
            if !declared.contains(n) {
                return Err(field_err(name, "volumes", format!("named volume '{n}' is not declared in the top-level volumes")));
            }
        }
        out.push(mount);
    }
    Ok(out)
}

fn convert_depends_on(name: &str, v: Option<Value>) -> Result<BTreeMap<String, Condition>, ConfigError> {
    let mut out = BTreeMap::new();
    match v {
        None | Some(Value::Null) => {}
        Some(Value::Sequence(items)) => {
            for item in items {
                let dep = item.as_str().ok_or_else(|| field_err(name, "depends_on", "entries must be service names"))?;
                out.insert(dep.to_string(), Condition::Started);
            }
        }
        Some(Value::Mapping(map)) => {
            for (k, v) in map {
                let dep = k.as_str().ok_or_else(|| field_err(name, "depends_on", "names must be strings"))?.to_string();
                let mut condition = Condition::Started;
                match v {
                    Value::Null => {}
                    Value::Mapping(m) => {
                        for (ck, cv) in m {
                            match (ck.as_str(), cv.as_str()) {
                                (Some("condition"), Some("service_started")) => condition = Condition::Started,
                                (Some("condition"), Some("service_healthy")) => condition = Condition::Healthy,
                                (Some("condition"), Some(other)) => {
                                    return Err(field_err(name, "depends_on", format!("condition '{other}' is not supported (service_started or service_healthy)")))
                                }
                                (Some(other), _) => {
                                    return Err(field_err(name, "depends_on", format!("key '{other}' of '{dep}' is not supported")))
                                }
                                _ => return Err(field_err(name, "depends_on", "invalid entry")),
                            }
                        }
                    }
                    _ => return Err(field_err(name, "depends_on", format!("'{dep}' must be empty or a map with 'condition'"))),
                }
                out.insert(dep, condition);
            }
        }
        Some(_) => return Err(field_err(name, "depends_on", "must be a list or a map")),
    }
    Ok(out)
}

/// `None` when the check is disabled (`test: ["NONE"]`).
fn convert_health(name: &str, h: RawHealthcheck) -> Result<Option<Health>, ConfigError> {
    if let Some(key) = h.extra.keys().next() {
        return Err(ConfigError::UnsupportedKey { scope: format!("service '{name}' healthcheck"), key: key.clone() });
    }
    let test = h.test.ok_or_else(|| field_err(name, "healthcheck", "test is required"))?;
    let cmd = match test {
        Words::Line(s) => s,
        Words::List(items) => {
            let items: Vec<String> = items
                .iter()
                .map(|v| scalar(v).ok_or_else(|| field_err(name, "healthcheck", "test items must be scalars")))
                .collect::<Result<_, _>>()?;
            match items.split_first() {
                Some((kind, [])) if kind == "NONE" => return Ok(None),
                Some((kind, rest)) if kind == "CMD-SHELL" && rest.len() == 1 => rest[0].clone(),
                Some((kind, rest)) if kind == "CMD" && !rest.is_empty() => {
                    rest.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
                }
                _ => return Err(field_err(name, "healthcheck", "test must be [\"CMD-SHELL\", cmd], [\"CMD\", args...], [\"NONE\"] or a string")),
            }
        }
    };
    if cmd.trim().is_empty() {
        return Err(field_err(name, "healthcheck", "test must not be empty"));
    }
    let interval = match h.interval {
        Some(s) => parse_duration(&s).ok_or_else(|| ConfigError::InvalidDuration { service: name.into(), value: s })?,
        None => DEFAULT_HEALTH_INTERVAL,
    };
    Ok(Some(Health { cmd, interval }))
}

#[cfg(test)]
mod tests;
