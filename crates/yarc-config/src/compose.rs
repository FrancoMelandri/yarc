//! Reader for Docker Compose files, filling the same [`Project`] model as `yarc.yaml`.
//!
//! Supported per service: `image`, `command`, `entrypoint`, `environment` (list or map),
//! `ports` (short form), `volumes` (short form), `depends_on` (list or map with
//! `condition`), `user`, `working_dir`, `healthcheck` (`test`, `interval`),
//! `container_name`, `secrets` (short form, file secrets), `profiles`, `stop_grace_period`
//! `restart`, and the limits `mem_limit`, `memswap_limit`, `cpus`, `pids_limit` and
//! `deploy.resources.limits` (`memory`, `cpus`, `pids`; `reservations` is accepted and ignored,
//! there is nothing to reserve). A service may carry an `x-yarc:` block with yarc-only settings (`user`). Top-level `volumes` may only declare names. `x-...` keys and the
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
use crate::{
    validate, Condition, ConfigError, Health, PortMapping, Project, ResourceError, Resources, RestartPolicy,
    SecretMount, Service, VolumeSource,
};

#[derive(Debug, Deserialize)]
struct RawCompose {
    #[serde(default)]
    services: BTreeMap<String, RawService>,
    #[serde(default)]
    volumes: Option<BTreeMap<String, Value>>,
    secrets: Option<BTreeMap<String, RawSecret>>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct RawSecret {
    file: Option<String>,
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
    #[serde(default)]
    secrets: Vec<Value>,
    #[serde(default)]
    profiles: Vec<String>,
    stop_grace_period: Option<String>,
    restart: Option<Value>,
    mem_limit: Option<Value>,
    memswap_limit: Option<Value>,
    cpus: Option<Value>,
    pids_limit: Option<Value>,
    deploy: Option<Value>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

/// The `x-yarc:` block of a service: settings compose has no place for.
#[derive(Debug, Deserialize)]
struct RawYarcExtension {
    user: Option<Value>,
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

    let mut secret_files = BTreeMap::new();
    for (name, secret) in raw.secrets.unwrap_or_default() {
        if !valid_name(&name) {
            return Err(field_err(
                "",
                &format!("secrets.{name}"),
                "names may only contain letters, digits, '_', '.' and '-'",
            ));
        }
        if let Some(key) = secret.extra.keys().next() {
            return Err(ConfigError::UnsupportedKey { scope: format!("secret '{name}'"), key: key.clone() });
        }
        let file = secret
            .file
            .ok_or_else(|| field_err("", &format!("secrets.{name}"), "only file secrets are supported (file: PATH)"))?;
        if file.trim().is_empty() {
            return Err(field_err("", &format!("secrets.{name}.file"), "must not be empty"));
        }
        secret_files.insert(name, file);
    }

    let mut services = BTreeMap::new();
    for (name, svc) in raw.services {
        services.insert(name.clone(), convert_service(&name, svc, &declared_volumes, &secret_files, lookup)?);
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
            *s = interpolate(s, lookup)
                .map_err(|e| ConfigError::Interpolation { path: path.to_string(), reason: e.to_string() })?;
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
    let safe = !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c));
    if safe {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

fn convert_service<F>(
    name: &str,
    raw: RawService,
    declared_volumes: &[String],
    secret_files: &BTreeMap<String, String>,
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
    let ext: Option<RawYarcExtension> = raw
        .extra
        .get("x-yarc")
        .map(|v| serde_yaml_ng::from_value(v.clone()).map_err(|e| field_err(name, "x-yarc", e.to_string())))
        .transpose()?;
    if let Some(key) = ext.as_ref().and_then(|e| e.extra.keys().next()) {
        return Err(ConfigError::UnsupportedKey { scope: format!("service '{name}' x-yarc"), key: key.clone() });
    }
    // x-yarc is yarc-specific, so it wins over the generic key
    let user_value = ext.and_then(|e| e.user).or(raw.user);
    let image = raw.image.ok_or_else(|| field_err(name, "image", "is required"))?;
    if image.trim().is_empty() {
        return Err(ConfigError::EmptyImage(name.into()));
    }

    let cmd = raw.command.map(|w| words(name, "command", w)).transpose()?.unwrap_or_default();
    let entrypoint = raw.entrypoint.map(|w| words(name, "entrypoint", w)).transpose()?;

    let user = match user_value {
        None => None,
        Some(v) => {
            let u = scalar(&v)
                .map(|s| s.trim().to_string())
                .ok_or_else(|| field_err(name, "user", "must be a string or number"))?;
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
    let secrets = convert_secrets(name, &raw.secrets, secret_files)?;
    for p in &raw.profiles {
        if !valid_name(p) {
            return Err(field_err(name, "profiles", format!("invalid profile name '{p}'")));
        }
    }
    let restart = match raw.restart {
        None => RestartPolicy::No,
        // YAML 1.1 readers turn an unquoted `no` into false; accept that spelling too
        Some(Value::Bool(false)) => RestartPolicy::No,
        Some(Value::String(text)) => {
            text.parse().map_err(|e: crate::ParsePolicyError| field_err(name, "restart", e.to_string()))?
        }
        Some(_) => return Err(field_err(name, "restart", "must be one of no, always, unless-stopped, on-failure[:N]")),
    };
    let resources =
        convert_resources(name, [&raw.mem_limit, &raw.memswap_limit, &raw.cpus, &raw.pids_limit], &raw.deploy)?;
    let stop_grace_period = raw
        .stop_grace_period
        .map(|s| parse_duration(&s).ok_or_else(|| ConfigError::InvalidDuration { service: name.into(), value: s }))
        .transpose()?;
    let health = raw.healthcheck.map(|h| convert_health(name, h)).transpose()?.flatten();

    Ok(Service {
        image,
        cmd,
        entrypoint,
        working_dir: raw.working_dir,
        container_name: raw.container_name,
        secrets,
        profiles: raw.profiles,
        stop_grace_period,
        restart,
        resources,
        user,
        env,
        ports,
        volumes,
        health,
        needs,
    })
}

/// The limits of a service, from the service-level keys and/or `deploy.resources.limits`
/// (naming the same limit twice with different values is an error).
fn convert_resources(name: &str, keys: [&Option<Value>; 4], deploy: &Option<Value>) -> Result<Resources, ConfigError> {
    // (limit, where it was written) for each of memory, swap, cpus, pids
    let mut found: [Option<(String, String)>; 4] = Default::default();
    let mut put = |slot: usize, key: &str, v: &Value| -> Result<(), ConfigError> {
        let text = scalar(v).ok_or_else(|| field_err(name, key, "must be a number or a size such as 512m"))?;
        match &found[slot] {
            Some((other, _)) if *other != text => {
                Err(field_err(name, key, "is also set elsewhere with a different value"))
            }
            Some(_) => Ok(()),
            None => {
                found[slot] = Some((text, key.to_string()));
                Ok(())
            }
        }
    };
    for (slot, key) in ["mem_limit", "memswap_limit", "cpus", "pids_limit"].into_iter().enumerate() {
        if let Some(v) = keys[slot] {
            put(slot, key, v)?;
        }
    }
    if let Some(deploy) = deploy {
        let unsupported = |scope: &str, key: &str| ConfigError::UnsupportedKey {
            scope: format!("service '{name}' {scope}"),
            key: key.to_string(),
        };
        let map = |v: &Value, key: &str| -> Result<serde_yaml_ng::Mapping, ConfigError> {
            match v {
                Value::Null => Ok(Default::default()),
                Value::Mapping(m) => Ok(m.clone()),
                _ => Err(field_err(name, key, "must be a mapping")),
            }
        };
        for (k, v) in map(deploy, "deploy")? {
            let k = k.as_str().unwrap_or("?").to_string();
            if k != "resources" {
                return Err(unsupported("deploy", &k));
            }
            for (k, v) in map(&v, "deploy.resources")? {
                match k.as_str().unwrap_or("?") {
                    "limits" => {
                        for (k, v) in map(&v, "deploy.resources.limits")? {
                            let k = k.as_str().unwrap_or("?").to_string();
                            let slot = match k.as_str() {
                                "memory" => 0,
                                "cpus" => 2,
                                "pids" => 3,
                                _ => return Err(unsupported("deploy.resources.limits", &k)),
                            };
                            put(slot, &format!("deploy.resources.limits.{k}"), &v)?;
                        }
                    }
                    "reservations" => {
                        for (k, _) in map(&v, "deploy.resources.reservations")? {
                            let k = k.as_str().unwrap_or("?");
                            if !matches!(k, "memory" | "cpus") {
                                return Err(unsupported("deploy.resources.reservations", k));
                            }
                        }
                    }
                    other => return Err(unsupported("deploy.resources", other)),
                }
            }
        }
    }
    let at = |slot: usize| found[slot].clone();
    let wrap = |key: &str, e: ResourceError| field_err(name, key, e.reason);
    let parse = |slot: usize, f: fn(&str) -> Result<u64, ResourceError>| -> Result<Option<u64>, ConfigError> {
        at(slot).map(|(text, key)| f(&text).map_err(|e| wrap(&key, e))).transpose()
    };
    let resources = Resources {
        memory: parse(0, crate::parse_memory)?,
        memory_swap: parse(1, crate::parse_memory_swap)?,
        cpus_millis: at(2).map(|(t, key)| crate::parse_cpus(&t).map_err(|e| wrap(&key, e))).transpose()?,
        pids: at(3).map(|(t, key)| crate::parse_pids(&t).map_err(|e| wrap(&key, e))).transpose()?.flatten(),
    };
    let swap_key = found[1].as_ref().map_or("memswap_limit", |(_, k)| k.as_str());
    resources.check().map_err(|e| wrap(swap_key, e))?;
    Ok(resources)
}

fn convert_environment<F>(name: &str, env: Option<Value>, lookup: &F) -> Result<BTreeMap<String, String>, ConfigError>
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
                let s =
                    scalar(&item).ok_or_else(|| field_err(name, "environment", "entries must be KEY=value strings"))?;
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
                        let v = scalar(&v)
                            .ok_or_else(|| field_err(name, "environment", format!("value of {k} must be a scalar")))?;
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
            return Err(field_err(
                name,
                "ports",
                format!("'{s}': a bare container port needs a random host port, which is not supported"),
            ))
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

fn convert_volumes(name: &str, volumes: &[Value], declared: &[String]) -> Result<Vec<crate::VolumeMount>, ConfigError> {
    let mut out = Vec::new();
    for v in volumes {
        let Value::String(s) = v else {
            return Err(field_err(name, "volumes", "only the short \"SOURCE:TARGET[:ro]\" syntax is supported"));
        };
        // `:rw` is compose's explicit default; yarc.yaml has no spelling for it
        let mount = parse_volume(name, s.strip_suffix(":rw").unwrap_or(s))?;
        if let VolumeSource::Named(n) = &mount.source {
            if !declared.contains(n) {
                return Err(field_err(
                    name,
                    "volumes",
                    format!("named volume '{n}' is not declared in the top-level volumes"),
                ));
            }
        }
        out.push(mount);
    }
    Ok(out)
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn convert_secrets(
    name: &str,
    refs: &[Value],
    declared: &BTreeMap<String, String>,
) -> Result<Vec<SecretMount>, ConfigError> {
    let mut out: Vec<SecretMount> = Vec::new();
    for r in refs {
        let Value::String(secret) = r else {
            return Err(field_err(name, "secrets", "only the short form (a list of secret names) is supported"));
        };
        let file = declared.get(secret).ok_or_else(|| {
            field_err(name, "secrets", format!("secret '{secret}' is not declared in the top-level secrets"))
        })?;
        if !out.iter().any(|s| &s.name == secret) {
            out.push(SecretMount { name: secret.clone(), file: file.clone() });
        }
    }
    Ok(out)
}

fn convert_depends_on(name: &str, v: Option<Value>) -> Result<BTreeMap<String, Condition>, ConfigError> {
    let mut out = BTreeMap::new();
    match v {
        None | Some(Value::Null) => {}
        Some(Value::Sequence(items)) => {
            for item in items {
                let dep =
                    item.as_str().ok_or_else(|| field_err(name, "depends_on", "entries must be service names"))?;
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
                                    return Err(field_err(
                                        name,
                                        "depends_on",
                                        format!(
                                            "condition '{other}' is not supported (service_started or service_healthy)"
                                        ),
                                    ))
                                }
                                (Some(other), _) => {
                                    return Err(field_err(
                                        name,
                                        "depends_on",
                                        format!("key '{other}' of '{dep}' is not supported"),
                                    ))
                                }
                                _ => return Err(field_err(name, "depends_on", "invalid entry")),
                            }
                        }
                    }
                    _ => {
                        return Err(field_err(
                            name,
                            "depends_on",
                            format!("'{dep}' must be empty or a map with 'condition'"),
                        ))
                    }
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
                _ => {
                    return Err(field_err(
                        name,
                        "healthcheck",
                        "test must be [\"CMD-SHELL\", cmd], [\"CMD\", args...], [\"NONE\"] or a string",
                    ))
                }
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
