//! Compose-style projects: `up`, `down` and `project ps|logs|stop|config`.
//!
//! A project is the set of containers labelled `yarc.project=<name>`; the
//! containers of a service are named `<project>-<service>-1`. `up` is
//! idempotent: a service whose container exists with the same definition hash
//! (label `yarc.config-hash`) is left alone (or just started if it had stopped),
//! a service whose definition changed is recreated.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use yarc_config::{Condition, PortMapping, Project, Protocol, Service};
use yarc_oci::Digest;
use yarc_runtime::{ContainerSpec, ContainerStore, PortMap, ProjectNet, RuntimeError};

pub const LABEL_PROJECT: &str = "yarc.project";
pub const LABEL_SERVICE: &str = "yarc.service";
pub const LABEL_HASH: &str = "yarc.config-hash";

const MAX_PROJECT_NAME: usize = 40;
const MAX_CONTAINER_NAME: usize = 64;

/// The global `--file` / `--project` options.
#[derive(Debug, Clone)]
pub struct Opts {
    pub file: PathBuf,
    pub name: Option<String>,
    /// Active compose profiles.
    pub profiles: Vec<String>,
    /// `--env-file`s; when empty the `.env` next to the project file is used.
    pub env_files: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct Loaded {
    pub name: String,
    pub file: PathBuf,
    pub project: Project,
    /// Service names, dependencies first.
    pub order: Vec<String>,
    /// Services left out because their profile is not active, with those profiles.
    pub skipped: Vec<(String, Vec<String>)>,
}

// ------------------------------------------------------------------ pure parts

/// One line per service left out because its profile is not active.
pub fn skipped_notes(skipped: &[(String, Vec<String>)]) -> Vec<String> {
    skipped
        .iter()
        .map(|(service, profiles)| {
            let list = profiles.iter().map(|p| format!("--profile {p}")).collect::<Vec<_>>().join(" or ");
            format!("{service}: not started, it belongs to profile {} (use {list} to include it)", profiles.join(", "))
        })
        .collect()
}

/// `My App!` → `my-app`. `None` when nothing usable is left.
pub fn sanitize_project_name(raw: &str) -> Option<String> {
    let mapped: String = raw
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect();
    let name: String = mapped.trim_start_matches(['-', '_']).chars().take(MAX_PROJECT_NAME).collect();
    (!name.is_empty()).then_some(name)
}

pub fn validate_project_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let ok = name.len() <= MAX_PROJECT_NAME
        && chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        bail!("invalid project name '{name}': use lowercase letters, digits, '_' and '-' (max {MAX_PROJECT_NAME}, starting with a letter or digit)")
    }
}

pub fn container_name(project: &str, service: &str) -> String {
    format!("{project}-{service}-1")
}

fn check_service_name(project: &str, service: &str) -> Result<()> {
    let mut chars = service.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !ok {
        bail!(
            "invalid service name '{service}': use letters, digits, '_', '.' and '-', starting with a letter or digit"
        );
    }
    if container_name(project, service).len() > MAX_CONTAINER_NAME {
        bail!(
            "service name '{service}' is too long: '{}' would exceed {MAX_CONTAINER_NAME} characters",
            container_name(project, service)
        );
    }
    Ok(())
}

/// Dependencies first; among services that are ready, alphabetical (so the
/// order is deterministic). Configuration validation already rejects cycles.
pub fn start_order(project: &Project) -> Result<Vec<String>> {
    let mut remaining: BTreeMap<&str, Vec<&str>> = project
        .services
        .iter()
        .map(|(name, svc)| (name.as_str(), svc.needs.keys().map(String::as_str).collect()))
        .collect();
    let mut order = Vec::new();
    while let Some(next) =
        remaining.iter().find(|(_, deps)| deps.iter().all(|d| !remaining.contains_key(d))).map(|(name, _)| *name)
    {
        remaining.remove(next);
        order.push(next.to_string());
    }
    if remaining.is_empty() {
        Ok(order)
    } else {
        bail!("dependency cycle between services: {}", remaining.keys().copied().collect::<Vec<_>>().join(", "))
    }
}

/// The TCP ports the project publishes on `127.0.0.1`, sorted. A host port can
/// only be published once. (UDP mappings are accepted in the file but not
/// published.)
pub fn published_ports(project: &Project) -> Result<Vec<PortMap>> {
    let mut seen: BTreeMap<u16, &str> = BTreeMap::new();
    let mut maps = Vec::new();
    for (name, svc) in &project.services {
        for p in svc.ports.iter().filter(|p| p.protocol == Protocol::Tcp) {
            if let Some(other) = seen.insert(p.host, name) {
                bail!("host port {} is published by both {other} and {name}", p.host);
            }
            maps.push(PortMap { host: p.host, container: p.container });
        }
    }
    maps.sort();
    Ok(maps)
}

/// `127.0.0.1:8080->80/tcp, 5353->53/udp (not published)` for listings.
pub fn format_ports(ports: &[PortMapping]) -> String {
    ports
        .iter()
        .map(|p| match p.protocol {
            Protocol::Tcp => format!("127.0.0.1:{}->{}/tcp", p.host, p.container),
            Protocol::Udp => format!("{}->{}/udp (not published)", p.host, p.container),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Bump when the way containers are wired up changes in a way that requires
/// recreating them (1: host network, 2: shared project network, 3: volumes, 4: resolv.conf).
const WIRING_MODEL: u32 = 4;

/// Lexically normalizes a path: drops `.` and resolves `..` (no symlink lookups,
/// the path may not exist yet).
fn clean_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Above the root there is nothing: `/..` is `/`. A relative path
                // may start with `..`s.
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `pgdata:/var/lib/postgresql/data:ro` as written in the file.
pub fn format_volume(v: &yarc_config::VolumeMount) -> String {
    let source = match &v.source {
        yarc_config::VolumeSource::Host(p) | yarc_config::VolumeSource::Named(p) => p,
    };
    format!("{source}:{}{}", v.target, if v.read_only { ":ro" } else { "" })
}

/// Where the data of a volume lives on the host, and where it appears in the
/// container. Relative host paths are relative to the project file's directory,
/// `~` is the home directory, named volumes live in `<volumes_dir>/<project>_<name>`.
pub fn resolve_volume(
    base: &Path,
    volumes_dir: &Path,
    project: &str,
    home: Option<&Path>,
    v: &yarc_config::VolumeMount,
) -> Result<yarc_runtime::VolumeSpec> {
    let invalid = |why: &str| anyhow!("volume '{}': {why}", format_volume(v));
    if v.target == "/" || Path::new(&v.target).components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(invalid("the target must be a directory or file inside the container, without '..'"));
    }
    let source = match &v.source {
        yarc_config::VolumeSource::Named(name) => {
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) || name.starts_with('.')
            {
                return Err(invalid("a volume name may only contain letters, digits, '_', '.' and '-'"));
            }
            volumes_dir.join(format!("{project}_{name}"))
        }
        yarc_config::VolumeSource::Host(path) => {
            let p = if path == "~" || path.starts_with("~/") {
                let home = home.ok_or_else(|| invalid("'~' needs the HOME environment variable"))?;
                home.join(path.trim_start_matches('~').trim_start_matches('/'))
            } else if path.starts_with('/') {
                PathBuf::from(path)
            } else {
                base.join(path)
            };
            clean_path(&p)
        }
    };
    Ok(yarc_runtime::VolumeSpec { source, target: v.target.clone(), read_only: v.read_only })
}

/// Resolves every volume of a service and creates the host directories that do
/// not exist yet (like `docker run -v` does).
#[cfg(target_os = "linux")]
fn prepare_volumes(root: &Path, loaded: &Loaded, def: &Service) -> Result<Vec<yarc_runtime::VolumeSpec>> {
    let base = loaded.file.parent().unwrap_or(Path::new("."));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut specs = Vec::new();
    for v in &def.volumes {
        let spec = resolve_volume(base, &root.join("volumes"), &loaded.name, home.as_deref(), v)?;
        if !spec.source.exists() {
            std::fs::create_dir_all(&spec.source)
                .with_context(|| format!("cannot create {}", spec.source.display()))?;
        }
        specs.push(spec);
    }
    for (secret, source) in secret_files(loaded, def)? {
        specs.push(yarc_runtime::VolumeSpec {
            source,
            target: format!("/run/secrets/{}", secret.name),
            read_only: true,
        });
    }
    Ok(specs)
}

/// The host file of each of the service's secrets (relative to the project file's
/// directory, `~` allowed). A missing file is an error naming the secret.
#[cfg(target_os = "linux")]
fn secret_files<'a>(loaded: &Loaded, def: &'a Service) -> Result<Vec<(&'a yarc_config::SecretMount, PathBuf)>> {
    let base = loaded.file.parent().unwrap_or(Path::new("."));
    let mut out = Vec::new();
    for secret in &def.secrets {
        let path = match secret.file.strip_prefix("~/") {
            Some(rest) => std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(rest))
                .ok_or_else(|| anyhow!("secret '{}': cannot expand '~' without HOME", secret.name))?,
            None if Path::new(&secret.file).is_absolute() => PathBuf::from(&secret.file),
            None => base.join(&secret.file),
        };
        let path = clean_path(&path);
        if !path.is_file() {
            bail!("secret '{}': file {} does not exist (or is not a regular file)", secret.name, path.display());
        }
        out.push((secret, path));
    }
    Ok(out)
}

/// Fails before anything is created when a secret file is missing.
#[cfg(target_os = "linux")]
fn check_secrets(loaded: &Loaded) -> Result<()> {
    for name in &loaded.order {
        secret_files(loaded, &loaded.project.services[name]).with_context(|| format!("service '{name}'"))?;
    }
    Ok(())
}

/// Host directories of the project's named volumes (what `down -v` deletes).
pub fn named_volume_dirs(root: &Path, loaded: &Loaded) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = loaded
        .project
        .services
        .values()
        .flat_map(|s| s.volumes.iter())
        .filter_map(|v| match &v.source {
            yarc_config::VolumeSource::Named(n) => Some(root.join("volumes").join(format!("{}_{n}", loaded.name))),
            yarc_config::VolumeSource::Host(_) => None,
        })
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Hash of everything that, when changed, requires a new container. `needs` is
/// left out on purpose: it only affects start order.
pub fn service_hash(service: &Service) -> String {
    let mut canonical = format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|wiring{WIRING_MODEL}",
        service.image, service.cmd, service.env, service.ports, service.volumes, service.health
    );
    // Only when set, so containers made before `user` existed keep their hash.
    if let Some(user) = &service.user {
        canonical.push_str(&format!("|user:{user}"));
    }
    if let Some(entrypoint) = &service.entrypoint {
        canonical.push_str(&format!("|entrypoint:{entrypoint:?}"));
    }
    if let Some(dir) = &service.working_dir {
        canonical.push_str(&format!("|workdir:{dir}"));
    }
    if !service.secrets.is_empty() {
        canonical.push_str(&format!("|secrets:{:?}", service.secrets));
    }
    if !service.restart.is_no() {
        canonical.push_str(&format!("|restart:{}", service.restart));
    }
    Digest::of(canonical.as_bytes()).hex().to_string()
}

pub fn labels(project: &str, service: &str, hash: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (LABEL_PROJECT.to_string(), project.to_string()),
        (LABEL_SERVICE.to_string(), service.to_string()),
        (LABEL_HASH.to_string(), hash.to_string()),
    ])
}

/// Sorts a project's containers by the service order (`descending` for
/// shutdown); containers of services that are no longer in the file ("orphans")
/// come first when stopping and last otherwise.
pub fn sort_by_service_order(specs: &mut [ContainerSpec], order: &[String], descending: bool) {
    let position =
        |spec: &ContainerSpec| spec.labels.get(LABEL_SERVICE).and_then(|s| order.iter().position(|o| o == s));
    specs.sort_by(|a, b| {
        let (pa, pb) = (position(a), position(b));
        let ord = match (pa, pb) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.id.cmp(&b.id),
        };
        if descending {
            ord.reverse()
        } else {
            ord
        }
    });
}

// ------------------------------------------------------------------- loading

pub fn project_name(opts: &Opts) -> Result<String> {
    if let Some(name) = &opts.name {
        validate_project_name(name)?;
        return Ok(name.clone());
    }
    let file = std::path::absolute(&opts.file).with_context(|| format!("cannot resolve {}", opts.file.display()))?;
    file.parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .and_then(sanitize_project_name)
        .ok_or_else(|| anyhow!("cannot derive a project name from {}; pass --project", file.display()))
}

/// Names tried, in order, when `--file` was left at its default and `yarc.yaml` does not exist.
const COMPOSE_FILE_NAMES: [&str; 5] =
    ["yarc.yml", "compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"];
const DEFAULT_FILE: &str = "yarc.yaml";

/// The project file to use: the requested one, or (only for the default name) the first
/// of the usual names that exists in the current directory.
pub fn resolve_file(requested: &Path) -> PathBuf {
    if requested == Path::new(DEFAULT_FILE) && !requested.exists() {
        if let Some(found) = discover_in(Path::new(".")) {
            return PathBuf::from(found);
        }
    }
    requested.to_path_buf()
}

fn discover_in(dir: &Path) -> Option<&'static str> {
    COMPOSE_FILE_NAMES.into_iter().find(|n| dir.join(n).is_file())
}

/// `yarc.yaml` / `yarc.yml` use the native format; any other name is read as a Compose file.
fn is_compose_file(file: &Path) -> bool {
    !file.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("yarc."))
}

/// Variables for compose interpolation, from the `--env-file`s (later ones win) or, when none
/// were given, the `.env` next to the project file if there is one. Like Docker Compose, an
/// explicit `--env-file` replaces `.env`, and must exist. The process environment is
/// consulted first by the caller, so it always wins.
fn compose_variables(dir: &Path, env_files: &[PathBuf]) -> Result<BTreeMap<String, String>> {
    let files: Vec<PathBuf> = if env_files.is_empty() {
        Some(dir.join(".env")).filter(|p| p.is_file()).into_iter().collect()
    } else {
        env_files.to_vec()
    };
    let mut merged = BTreeMap::new();
    for file in files {
        let path = std::path::absolute(&file).with_context(|| format!("cannot resolve {}", file.display()))?;
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("cannot read env file {}", path.display()))?;
        let vars =
            yarc_config::parse_dotenv(&text, &|n: &str| std::env::var(n).ok().or_else(|| merged.get(n).cloned()))
                .with_context(|| path.display().to_string())?;
        merged.extend(vars);
    }
    Ok(merged)
}

fn parse_project_file(file: &Path, text: &str, dotenv: &BTreeMap<String, String>) -> Result<Project> {
    let parsed = if is_compose_file(file) {
        yarc_config::parse_compose(text, &|n: &str| std::env::var(n).ok().or_else(|| dotenv.get(n).cloned()))
    } else {
        yarc_config::parse(text)
    };
    parsed.with_context(|| file.display().to_string())
}

/// The profiles to activate: `--profile` flags, else `COMPOSE_PROFILES` from the process
/// environment (`process_set`: it is set, even if empty, which then means none), else
/// `COMPOSE_PROFILES` from the env files, like Docker Compose, whose installers write it
/// into `.env`. Comma separated; empty entries are ignored.
fn active_profiles(requested: &[String], process_set: bool, dotenv: &BTreeMap<String, String>) -> Vec<String> {
    let split = |items: &mut dyn Iterator<Item = &str>| -> Vec<String> {
        items.map(str::trim).filter(|p| !p.is_empty()).map(String::from).collect()
    };
    let from_flags = split(&mut requested.iter().map(String::as_str));
    if !from_flags.is_empty() || process_set {
        return from_flags;
    }
    split(&mut dotenv.get("COMPOSE_PROFILES").into_iter().flat_map(|v| v.split(',')))
}

pub fn load(opts: &Opts) -> Result<Loaded> {
    let requested = resolve_file(&opts.file);
    let opts = &Opts { file: requested, ..opts.clone() };
    let name = project_name(opts)?;
    let file = std::path::absolute(&opts.file).with_context(|| format!("cannot resolve {}", opts.file.display()))?;
    let text = std::fs::read_to_string(&file).with_context(|| format!("cannot read {}", file.display()))?;
    let dotenv = if is_compose_file(&file) {
        compose_variables(file.parent().unwrap_or(Path::new(".")), &opts.env_files)?
    } else {
        BTreeMap::new()
    };
    let profiles = active_profiles(&opts.profiles, std::env::var_os("COMPOSE_PROFILES").is_some(), &dotenv);
    let full = parse_project_file(&file, &text, &dotenv)?;
    let skipped = full.inactive_services(&profiles);
    let project = full.with_profiles(&profiles).with_context(|| file.display().to_string())?;
    if project.services.is_empty() {
        bail!("{} defines no services", file.display());
    }
    for service in project.services.keys() {
        check_service_name(&name, service)?;
    }
    published_ports(&project)?; // duplicates are a mistake in the file
    let order = start_order(&project)?;
    Ok(Loaded { name, file, project, order, skipped })
}

pub fn project_containers(store: &ContainerStore, project: &str) -> Result<Vec<ContainerSpec>> {
    Ok(store.list()?.into_iter().filter(|c| c.labels.get(LABEL_PROJECT).is_some_and(|p| p == project)).collect())
}

/// Order for shutdown when the file may be gone: use it if it loads.
fn shutdown_order(opts: &Opts) -> Vec<String> {
    load(opts).map(|l| l.order).unwrap_or_default()
}

// ------------------------------------------------------- read-only commands

pub fn config(opts: &Opts) -> Result<()> {
    let loaded = load(opts)?;
    println!("project: {}", loaded.name);
    println!("file:    {}", loaded.file.display());
    for note in skipped_notes(&loaded.skipped) {
        println!("not included: {note}");
    }
    println!("services (start order):");
    let width = loaded.order.iter().map(String::len).max().unwrap_or(0);
    for name in &loaded.order {
        let svc = &loaded.project.services[name];
        let needs = if svc.needs.is_empty() {
            String::new()
        } else {
            let list: Vec<String> = svc
                .needs
                .iter()
                .map(|(d, c)| format!("{d}({})", if *c == Condition::Healthy { "healthy" } else { "started" }))
                .collect();
            format!("  needs: {}", list.join(", "))
        };
        println!("  {name:<width$}  {}  image {}{needs}", container_name(&loaded.name, name), svc.image);
        if let Some(alias) = &svc.container_name {
            println!("  {:<width$}  also known as {alias}", "");
        }
        if !svc.profiles.is_empty() {
            println!("  {:<width$}  profiles {}", "", svc.profiles.join(", "));
        }
        if !svc.restart.is_no() {
            println!("  {:<width$}  restart {}", "", svc.restart);
        }
        if let Some(grace) = svc.stop_grace_period {
            println!("  {:<width$}  stop grace period {}s", "", grace.as_secs());
        }
        if !svc.secrets.is_empty() {
            let names: Vec<&str> = svc.secrets.iter().map(|s| s.name.as_str()).collect();
            println!("  {:<width$}  secrets {}", "", names.join(", "));
        }
        if let Some(user) = &svc.user {
            println!("  {:<width$}  user {user}", "");
        }
        if let Some(entrypoint) = &svc.entrypoint {
            println!("  {:<width$}  entrypoint {entrypoint:?}", "");
        }
        if let Some(dir) = &svc.working_dir {
            println!("  {:<width$}  working dir {dir}", "");
        }
        if !svc.cmd.is_empty() {
            println!("  {:<width$}  command {:?}", "", svc.cmd);
        }
        if !svc.env.is_empty() {
            // names only: values may be secrets
            let names: Vec<&str> = svc.env.keys().map(String::as_str).collect();
            println!("  {:<width$}  env {}", "", names.join(", "));
        }
        if !svc.ports.is_empty() {
            println!("  {:<width$}  ports {}", "", format_ports(&svc.ports));
        }
        if !svc.volumes.is_empty() {
            let list: Vec<String> = svc.volumes.iter().map(format_volume).collect();
            println!("  {:<width$}  volumes {}", "", list.join(", "));
        }
    }
    Ok(())
}

pub fn ps(root: &Path, opts: &Opts) -> Result<()> {
    let name = project_name(opts)?;
    let store = ContainerStore::open(root)?;
    let mut specs = project_containers(&store, &name)?;
    let loaded = load(opts).ok();
    let order = loaded.as_ref().map(|l| l.order.clone()).unwrap_or_default();
    sort_by_service_order(&mut specs, &order, false);
    let mut rows = vec![["SERVICE".to_string(), "CONTAINER".into(), "STATUS".into(), "IMAGE".into(), "PORTS".into()]];
    for spec in &specs {
        let service = spec.labels.get(LABEL_SERVICE).map_or("-", String::as_str);
        let ports = loaded
            .as_ref()
            .and_then(|l| l.project.services.get(service))
            .map(|s| format_ports(&s.ports))
            .unwrap_or_default();
        rows.push([service.to_string(), spec.id.clone(), store.status_text(&spec.id)?, spec.image.clone(), ports]);
    }
    // columns as wide as their longest cell, so long service names do not break the table
    let widths: Vec<usize> = (0..4).map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0)).collect();
    for row in &rows {
        let line = format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3]
        );
        println!("{}", line.trim_end());
    }
    Ok(())
}

pub struct StreamOpts {
    pub tail: Option<usize>,
    pub timestamps: bool,
    pub follow: bool,
}

/// One container's log, read from `offset` on.
pub struct Source {
    pub label: String,
    pub id: String,
    pub offset: u64,
}

/// Prints the logs of several containers, each line prefixed with its label,
/// merged by timestamp. With `follow`, keeps going until none of the
/// containers is running any more. Returns `true` if `interrupted` ended it.
pub fn stream_logs(
    store: &ContainerStore,
    mut sources: Vec<Source>,
    opts: &StreamOpts,
    interrupted: &dyn Fn() -> bool,
) -> Result<bool> {
    let width = sources.iter().map(|s| s.label.len()).max().unwrap_or(0);
    let mut first = true;
    loop {
        let mut batch = Vec::new();
        let mut any_running = false;
        for src in &mut sources {
            // Check the lock *before* reading: whatever a container wrote before
            // it exited is then guaranteed to be in this read.
            any_running |= store.is_supervised(&src.id)?;
            let (entries, next) = yarc_runtime::read_log_from(&store.log_path(&src.id), src.offset)?;
            src.offset = next;
            let skip = match (first, opts.tail) {
                (true, Some(n)) => entries.len().saturating_sub(n),
                _ => 0,
            };
            batch.extend(entries.into_iter().skip(skip).map(|e| (src.label.clone(), e)));
        }
        batch.sort_by_key(|(_, e)| e.ts); // stable: ties keep the source order
        for (label, entry) in &batch {
            let line = if opts.timestamps {
                format!("{label:<width$} | {} {}", yarc_runtime::format_timestamp(entry.ts), entry.line)
            } else {
                format!("{label:<width$} | {}", entry.line)
            };
            match entry.stream {
                yarc_runtime::Stream::Stdout => println!("{line}"),
                yarc_runtime::Stream::Stderr => eprintln!("{line}"),
            }
        }
        first = false;
        if !opts.follow || !any_running {
            return Ok(false);
        }
        if interrupted() {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn logs(root: &Path, opts: &Opts, args: crate::cli::ProjectLogsArgs) -> Result<()> {
    let name = project_name(opts)?;
    let store = ContainerStore::open(root)?;
    let mut specs = project_containers(&store, &name)?;
    if specs.is_empty() {
        bail!("project {name} has no containers; run `yarc up` first");
    }
    for wanted in &args.services {
        if !specs.iter().any(|s| s.labels.get(LABEL_SERVICE) == Some(wanted)) {
            bail!("project {name} has no service '{wanted}'");
        }
    }
    if !args.services.is_empty() {
        specs.retain(|s| s.labels.get(LABEL_SERVICE).is_some_and(|svc| args.services.contains(svc)));
    }
    sort_by_service_order(&mut specs, &shutdown_order(opts), false);
    let sources = specs
        .iter()
        .map(|s| Source {
            label: s.labels.get(LABEL_SERVICE).cloned().unwrap_or_else(|| s.id.clone()),
            id: s.id.clone(),
            offset: 0,
        })
        .collect();
    let stream = StreamOpts { tail: args.tail, timestamps: args.timestamps, follow: args.follow };
    stream_logs(&store, sources, &stream, &|| false)?;
    Ok(())
}

// ------------------------------------------------------- commands needing Linux

#[cfg(target_os = "linux")]
mod interrupt {
    //! Ctrl-C handling for the foreground `up`: a flag the log loop polls.
    use std::sync::atomic::{AtomicBool, Ordering};

    static FLAG: AtomicBool = AtomicBool::new(false);

    extern "C" fn handler(_: libc::c_int) {
        FLAG.store(true, Ordering::SeqCst);
    }

    pub fn install() {
        let handler = handler as *const () as libc::sighandler_t;
        // SAFETY: the handler only stores to an atomic.
        unsafe {
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
        }
    }

    pub fn requested() -> bool {
        FLAG.load(Ordering::SeqCst)
    }
}

#[cfg(target_os = "linux")]
fn log_len(store: &ContainerStore, id: &str) -> u64 {
    std::fs::metadata(store.log_path(id)).map_or(0, |m| m.len())
}

#[cfg(target_os = "linux")]
enum Action {
    Create,
    Start,
    UpToDate,
    Recreate,
}

/// Brings one service up; returns the log offset from which this run's output
/// starts (so a foreground `up` does not replay earlier runs).
#[cfg(target_os = "linux")]
fn up_service(
    root: &Path,
    store: &ContainerStore,
    net: &ProjectNet,
    loaded: &Loaded,
    service: &str,
    def: &Service,
) -> Result<u64> {
    use crate::commands::{create_new, NewContainer};

    let project = loaded.name.as_str();
    // Directories for new volumes are created even when the container is only
    // started again: a volume may have been removed by hand in the meantime.
    let volumes = prepare_volumes(root, loaded, def)?;
    let cname = container_name(project, service);
    let hash = service_hash(def);
    let action = match store.load(&cname) {
        Ok(spec) if spec.labels.get(LABEL_HASH) == Some(&hash) => {
            if store.is_supervised(&cname)? {
                Action::UpToDate
            } else {
                Action::Start
            }
        }
        Ok(_) => Action::Recreate,
        Err(RuntimeError::NotFound(_)) => Action::Create,
        Err(e) => return Err(e.into()),
    };

    if let Action::Recreate = action {
        eprintln!("{service}: definition changed, recreating {cname}");
        yarc_runtime::stop_container(store, &cname, def.stop_grace_period.unwrap_or(DEFAULT_STOP_TIMEOUT))?;
        yarc_runtime::remove_container(store, &cname, true)?;
    }
    if matches!(action, Action::Create | Action::Recreate) {
        create_new(
            root,
            NewContainer {
                image: def.image.clone(),
                name: Some(cname.clone()),
                overrides: yarc_runtime::Overrides {
                    cmd: def.cmd.clone(),
                    entrypoint: def.entrypoint.clone(),
                    working_dir: def.working_dir.clone(),
                    user: def.user.clone(),
                    env: def.env.iter().map(|(k, v)| format!("{k}={v}")).collect(),
                },
                network: yarc_runtime::Network::Project,
                project_net: Some((net.dir().to_path_buf(), net.hosts_path())),
                volumes,
                labels: labels(project, service, &hash),
                health: def.health.as_ref().map(|h| yarc_runtime::HealthSpec::shell(&h.cmd, h.interval)),
                restart: def.restart,
            },
        )?;
    }
    let offset = log_len(store, &cname);
    match action {
        Action::UpToDate => eprintln!("{service}: {cname} is up to date"),
        Action::Start => {
            crate::detached::start(root, &cname, false, false)?;
            eprintln!("{service}: started {cname}");
        }
        Action::Create | Action::Recreate => {
            crate::detached::start(root, &cname, false, false)?;
            eprintln!("{service}: created and started {cname}");
        }
    }
    Ok(offset)
}

/// Makes sure the project's network exists with exactly the published ports of
/// the file. Containers join the network's namespaces when they start, so when
/// the network has to be replaced (ports changed, or its holder died) the
/// containers still running in the old one are stopped first; `up` then starts
/// them again in the new one.
#[cfg(target_os = "linux")]
fn ensure_network(root: &Path, store: &ContainerStore, loaded: &Loaded) -> Result<ProjectNet> {
    let wanted = published_ports(&loaded.project)?;
    let net = ProjectNet::new(root, &loaded.name)?;
    let current = if net.is_alive()? { net.info()? } else { None };
    if let Some(info) = &current {
        let mut have = info.published.clone();
        have.sort();
        if have == wanted {
            return Ok(net);
        }
    }

    let running: Vec<ContainerSpec> = project_containers(store, &loaded.name)?
        .into_iter()
        .filter(|c| store.is_supervised(&c.id).unwrap_or(false))
        .collect();
    if current.is_some() || !running.is_empty() {
        eprintln!(
            "recreating the project network ({})",
            if current.is_some() { "published ports changed" } else { "its holder is gone" }
        );
        let mut running = running;
        sort_by_service_order(&mut running, &loaded.order, true);
        stop_all(store, &running, &Grace::new(None, Some(&loaded.project)))?;
        yarc_runtime::stop_holder(&net, Duration::from_secs(5))?;
    }
    let pasta = crate::detached::find_pasta();
    crate::detached::start_holder(root, &loaded.name, &wanted, pasta.as_deref())
        .context("cannot create the project network")?;
    Ok(net)
}

/// Names the services use to reach each other: service names and container names.
#[cfg(target_os = "linux")]
fn host_names(loaded: &Loaded) -> Vec<String> {
    loaded
        .order
        .iter()
        .flat_map(|s| {
            let alias = loaded.project.services[s].container_name.clone();
            [Some(s.clone()), Some(container_name(&loaded.name, s)), alias.filter(|a| a != s)]
        })
        .flatten()
        .collect()
}

/// Warns about parts of the file that are accepted but not implemented yet.
#[cfg(target_os = "linux")]
fn warn_unsupported(loaded: &Loaded) {
    for name in &loaded.order {
        let svc = &loaded.project.services[name];
        if svc.ports.iter().any(|p| p.protocol == Protocol::Udp) {
            eprintln!("warning: {name}: only TCP ports are published; UDP mappings are ignored");
        }
    }
}

/// How long `up` waits for a dependency to become healthy: a generous start-up
/// allowance plus the time its health check needs to give up.
#[cfg(target_os = "linux")]
fn health_wait(def: &Service) -> Duration {
    let spec = def.health.as_ref().map(|h| yarc_runtime::HealthSpec::shell(&h.cmd, h.interval));
    Duration::from_secs(120)
        + spec.map_or(Duration::ZERO, |s| Duration::from_millis(s.interval_ms * u64::from(s.retries)))
}

/// Waits until `container` reports `healthy`. `Ok(false)` means `interrupted`
/// (Ctrl-C) ended the wait.
#[cfg(target_os = "linux")]
fn wait_healthy(
    store: &ContainerStore,
    service: &str,
    container: &str,
    timeout: Duration,
    interrupted: &dyn Fn() -> bool,
) -> Result<bool> {
    use yarc_runtime::{HealthStatus, Status};

    let deadline = std::time::Instant::now() + timeout;
    let mut announced = false;
    loop {
        match store.load_state(container)? {
            Some(state) if state.status == Status::Exited => {
                bail!("{service} exited (status {}) before it became healthy", state.exit_code.unwrap_or(-1));
            }
            Some(state) => match state.health.as_ref().map(|h| h.status) {
                Some(HealthStatus::Healthy) => return Ok(true),
                Some(HealthStatus::Unhealthy) => bail!("{service} is unhealthy"),
                Some(HealthStatus::Starting) => {}
                None => bail!("{service} has no health check"),
            },
            None => bail!("{service} was not started"),
        }
        if !store.is_supervised(container)? {
            bail!("{service} stopped supervising unexpectedly (it is dead)");
        }
        if interrupted() {
            return Ok(false);
        }
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting for {service} to become healthy");
        }
        if !announced {
            eprintln!("waiting for {service} to become healthy...");
            announced = true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait after SIGTERM: the `-t` option if given, else the service's
/// `stop_grace_period`, else 10 seconds (also for containers of removed services).
#[cfg(target_os = "linux")]
struct Grace {
    explicit: Option<Duration>,
    per_service: BTreeMap<String, Duration>,
}

#[cfg(target_os = "linux")]
impl Grace {
    fn new(explicit: Option<Duration>, project: Option<&Project>) -> Self {
        let per_service = project
            .into_iter()
            .flat_map(|p| p.services.iter())
            .filter_map(|(n, s)| s.stop_grace_period.map(|g| (n.clone(), g)))
            .collect();
        Grace { explicit, per_service }
    }

    fn for_spec(&self, spec: &ContainerSpec) -> Duration {
        self.explicit
            .or_else(|| spec.labels.get(LABEL_SERVICE).and_then(|s| self.per_service.get(s)).copied())
            .unwrap_or(DEFAULT_STOP_TIMEOUT)
    }
}

#[cfg(target_os = "linux")]
fn stop_all(store: &ContainerStore, specs: &[ContainerSpec], grace: &Grace) -> Result<()> {
    for spec in specs {
        yarc_runtime::stop_container(store, &spec.id, grace.for_spec(spec))?;
        eprintln!("{}: stopped", spec.id);
    }
    Ok(())
}

/// `yarc up`: returns the process exit code (130 when stopped with Ctrl-C).
#[cfg(target_os = "linux")]
pub fn up(root: &Path, opts: &Opts, detach: bool) -> Result<u8> {
    let loaded = load(opts)?;
    let store = ContainerStore::open(root)?;
    warn_unsupported(&loaded);
    for note in skipped_notes(&loaded.skipped) {
        eprintln!("note: {note}");
    }
    check_secrets(&loaded)?;
    if !detach {
        interrupt::install();
    }
    let net = ensure_network(root, &store, &loaded)?;
    net.write_hosts(&host_names(&loaded))?;
    let outbound = net.info()?.is_some_and(|i| i.outbound);
    net.write_resolv(outbound, &yarc_runtime::host_search_domains())?;
    if let Some(info) = net.info()? {
        if !info.outbound {
            eprintln!(
                "warning: the project has no outbound network ({}); services can only reach each other. \
                 Install passt (provides `pasta`) and recreate the network (`yarc down`, `yarc up`) to enable it",
                info.outbound_note.as_deref().unwrap_or("unknown reason")
            );
        }
    }

    let mut sources = Vec::new();
    for service in &loaded.order {
        if !detach && interrupt::requested() {
            break;
        }
        let def = &loaded.project.services[service];
        let mut interrupted = false;
        for (dep, condition) in &def.needs {
            if *condition == Condition::Healthy {
                let dep_def = &loaded.project.services[dep];
                let never = || false;
                let stop: &dyn Fn() -> bool = if detach { &never } else { &interrupt::requested };
                let ok = wait_healthy(&store, dep, &container_name(&loaded.name, dep), health_wait(dep_def), stop)
                    .with_context(|| format!("service {service} needs {dep} to be healthy"))?;
                interrupted |= !ok;
            }
        }
        if interrupted {
            break;
        }
        let offset = up_service(root, &store, &net, &loaded, service, &loaded.project.services[service])
            .with_context(|| format!("service {service}"))?;
        sources.push(Source { label: service.clone(), id: container_name(&loaded.name, service), offset });
    }
    if detach {
        return Ok(0);
    }

    let interrupted = interrupt::requested()
        || stream_logs(
            &store,
            sources,
            &StreamOpts { tail: None, timestamps: false, follow: true },
            &interrupt::requested,
        )?;
    if interrupted {
        eprintln!("\nGracefully stopping...");
        let mut specs = project_containers(&store, &loaded.name)?;
        sort_by_service_order(&mut specs, &loaded.order, true);
        stop_all(&store, &specs, &Grace::new(None, Some(&loaded.project)))?;
        return Ok(130);
    }
    Ok(0)
}

/// `yarc down`: stops and removes every container of the project, dependents
/// first, including containers of services that were removed from the file.
#[cfg(target_os = "linux")]
pub fn down(root: &Path, opts: &Opts, timeout: Option<Duration>, volumes: bool) -> Result<()> {
    let name = project_name(opts)?;
    // Read the file first: without it we could not tell which volumes are ours.
    let named_volumes = if volumes {
        let loaded = load(opts).context("`down --volumes` needs the project file to know its volumes")?;
        named_volume_dirs(root, &loaded)
    } else {
        Vec::new()
    };
    let store = ContainerStore::open(root)?;
    let mut specs = project_containers(&store, &name)?;
    sort_by_service_order(&mut specs, &shutdown_order(opts), true);
    let grace = Grace::new(timeout, load(opts).ok().as_ref().map(|l| &l.project));
    let net = ProjectNet::new(root, &name)?;
    if specs.is_empty() && !net.dir().exists() {
        eprintln!("project {name}: nothing to remove");
    }
    for spec in &specs {
        yarc_runtime::stop_container(&store, &spec.id, grace.for_spec(spec))?;
        yarc_runtime::remove_container(&store, &spec.id, true)?;
        eprintln!("{}: stopped and removed", spec.id);
    }
    // The network goes last: the containers were using it.
    if net.dir().exists() {
        yarc_runtime::stop_holder(&net, Duration::from_secs(5))?;
        net.remove()?;
        eprintln!("project {name}: network removed");
    }
    for dir in named_volumes {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {
                eprintln!("project {name}: volume {} removed", dir.file_name().and_then(|n| n.to_str()).unwrap_or("?"))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow!("cannot remove volume {}: {e}", dir.display())),
        }
    }
    Ok(())
}

/// `yarc project stop`: like `down` but keeps the containers.
#[cfg(target_os = "linux")]
pub fn stop(root: &Path, opts: &Opts, timeout: Option<Duration>) -> Result<()> {
    let name = project_name(opts)?;
    let store = ContainerStore::open(root)?;
    let mut specs = project_containers(&store, &name)?;
    sort_by_service_order(&mut specs, &shutdown_order(opts), true);
    stop_all(&store, &specs, &Grace::new(timeout, load(opts).ok().as_ref().map(|l| &l.project)))
}

#[cfg(not(target_os = "linux"))]
mod unsupported {
    use std::path::Path;
    use std::time::Duration;

    use anyhow::Result;

    use super::Opts;

    const MSG: &str = "running projects needs Linux; run yarc inside WSL";

    pub fn up(_root: &Path, _opts: &Opts, _detach: bool) -> Result<u8> {
        anyhow::bail!(MSG)
    }
    pub fn down(_root: &Path, _opts: &Opts, _timeout: Option<Duration>, _volumes: bool) -> Result<()> {
        anyhow::bail!(MSG)
    }
    pub fn stop(_root: &Path, _opts: &Opts, _timeout: Option<Duration>) -> Result<()> {
        anyhow::bail!(MSG)
    }
}
#[cfg(not(target_os = "linux"))]
pub use unsupported::{down, stop, up};

#[cfg(test)]
mod tests {
    use super::*;

    fn project(yaml: &str) -> Project {
        yarc_config::parse(yaml).unwrap()
    }

    #[cfg(target_os = "linux")]
    fn loaded_from(dir: &Path, yaml: &str) -> Loaded {
        let file = dir.join("docker-compose.yml");
        std::fs::write(&file, yaml).unwrap();
        load(&Opts { file, name: Some("demo".into()), profiles: vec![], env_files: vec![] }).unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn secret_files_resolve_against_the_project_dir_and_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets/tok.txt"), "x").unwrap();
        let yaml = "services:\n  a:\n    image: i\n    secrets: [tok]\nsecrets:\n  tok:\n    file: ./secrets/tok.txt\n";
        let loaded = loaded_from(dir.path(), yaml);
        let found = secret_files(&loaded, &loaded.project.services["a"]).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.name, "tok");
        assert_eq!(found[0].1, dir.path().join("secrets/tok.txt"));
        check_secrets(&loaded).unwrap();

        std::fs::remove_file(dir.path().join("secrets/tok.txt")).unwrap();
        let err = format!("{:#}", check_secrets(&loaded).unwrap_err());
        assert!(err.contains("service 'a'") && err.contains("secret 'tok'") && err.contains("does not exist"), "{err}");
        // a directory is not a secret file
        std::fs::create_dir(dir.path().join("secrets/tok.txt")).unwrap();
        assert!(check_secrets(&loaded).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stop_timeout_is_the_option_then_the_service_then_ten_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = loaded_from(
            dir.path(),
            "services:\n  slow:\n    image: i\n    stop_grace_period: 20s\n  fast:\n    image: i\n",
        );
        let spec = |service: &str| spec("c", Some(service));
        let grace = Grace::new(None, Some(&loaded.project));
        assert_eq!(grace.for_spec(&spec("slow")), Duration::from_secs(20));
        assert_eq!(grace.for_spec(&spec("fast")), Duration::from_secs(10));
        assert_eq!(grace.for_spec(&spec("removed-service")), Duration::from_secs(10));
        let forced = Grace::new(Some(Duration::from_secs(3)), Some(&loaded.project));
        assert_eq!(forced.for_spec(&spec("slow")), Duration::from_secs(3));
        assert_eq!(Grace::new(None, None).for_spec(&spec("slow")), Duration::from_secs(10));
    }

    #[test]
    fn the_hash_covers_secrets_but_not_profiles_or_grace() {
        let base = "services:\n  a:\n    image: i\n";
        let h = |extra: &str, top: &str| {
            let y = format!("{base}{extra}{top}");
            service_hash(&yarc_config::parse_compose(&y, &|_: &str| None).unwrap().services["a"])
        };
        let plain = h("", "");
        assert_eq!(plain, h("    profiles: [p]\n    stop_grace_period: 5s\n", ""));
        // a restart policy is part of the definition (changing it recreates the service) ...
        assert_ne!(plain, h("    restart: always\n", ""));
        assert_ne!(h("    restart: always\n", ""), h("    restart: on-failure:3\n", ""));
        // ... but spelling the default out is not a change
        assert_eq!(plain, h("    restart: \"no\"\n", ""));
        let with = h("    secrets: [s]\n", "secrets:\n  s:\n    file: ./f\n");
        assert_ne!(plain, with);
        assert_ne!(with, h("    secrets: [s]\n", "secrets:\n  s:\n    file: ./other\n"));
    }

    #[test]
    fn profiles_come_from_the_flags_first_then_from_compose_profiles_in_the_env_files() {
        let env = |v: &str| BTreeMap::from([("COMPOSE_PROFILES".to_string(), v.to_string())]);
        let none = BTreeMap::new();
        let list = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(active_profiles(&list(&["a"]), false, &env("b")), ["a"], "an explicit profile replaces the file's");
        assert_eq!(active_profiles(&[], false, &env("observability")), ["observability"]);
        assert_eq!(active_profiles(&[], false, &env("a, b ,,c")), ["a", "b", "c"]);
        assert!(active_profiles(&[], false, &env("")).is_empty(), "COMPOSE_PROFILES= in the file means none");
        assert!(active_profiles(&[], false, &none).is_empty());
        // set in the process environment (even empty): that wins over the files
        assert!(active_profiles(&[], true, &env("x")).is_empty());
        assert!(active_profiles(&list(&[""]), true, &env("x")).is_empty());
        assert_eq!(active_profiles(&list(&["p"]), true, &env("x")), ["p"]);
    }

    #[test]
    fn skipped_services_are_explained_with_the_flag_that_brings_them_back() {
        let one = skipped_notes(&[("dash".into(), vec!["observability".into()])]);
        assert_eq!(
            one,
            ["dash: not started, it belongs to profile observability (use --profile observability to include it)"]
        );
        let two = skipped_notes(&[("x".into(), vec!["a".into(), "b".into()])]);
        assert_eq!(two, ["x: not started, it belongs to profile a, b (use --profile a or --profile b to include it)"]);
        assert!(skipped_notes(&[]).is_empty());
    }

    #[test]
    fn discovery_prefers_the_documented_order() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(discover_in(dir.path()), None);
        std::fs::write(dir.path().join("docker-compose.yml"), "").unwrap();
        assert_eq!(discover_in(dir.path()), Some("docker-compose.yml"));
        std::fs::write(dir.path().join("compose.yaml"), "").unwrap();
        assert_eq!(discover_in(dir.path()), Some("compose.yaml"));
        assert!(is_compose_file(Path::new("docker-compose.yml")) && !is_compose_file(Path::new("/x/yarc.yaml")));
    }

    #[test]
    fn project_names_are_sanitized_or_validated() {
        assert_eq!(sanitize_project_name("My App!").as_deref(), Some("my-app-"));
        assert_eq!(sanitize_project_name("__demo").as_deref(), Some("demo"));
        assert_eq!(sanitize_project_name("Web.Shop_2").as_deref(), Some("web-shop_2"));
        assert_eq!(sanitize_project_name("---"), None);
        assert_eq!(sanitize_project_name(""), None);
        assert_eq!(sanitize_project_name(&"x".repeat(100)).unwrap().len(), MAX_PROJECT_NAME);

        for ok in ["demo", "a", "0day", "my_app-2"] {
            assert!(validate_project_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Demo", "-x", "_x", "a b", "a/b", &"x".repeat(41)] {
            assert!(validate_project_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn project_name_comes_from_the_option_or_the_directory() {
        let opts =
            Opts { file: PathBuf::from("/work/My Shop/yarc.yaml"), name: None, profiles: vec![], env_files: vec![] };
        assert_eq!(project_name(&opts).unwrap(), "my-shop");
        let opts = Opts {
            file: PathBuf::from("/work/shop/yarc.yaml"),
            name: Some("custom".into()),
            profiles: vec![],
            env_files: vec![],
        };
        assert_eq!(project_name(&opts).unwrap(), "custom");
        let opts = Opts {
            file: PathBuf::from("/work/shop/yarc.yaml"),
            name: Some("Bad Name".into()),
            profiles: vec![],
            env_files: vec![],
        };
        assert!(project_name(&opts).is_err());
    }

    #[test]
    fn container_and_service_names() {
        assert_eq!(container_name("demo", "web"), "demo-web-1");
        assert!(check_service_name("demo", "web.v2_x-1").is_ok());
        assert!(check_service_name("demo", "-web").is_err());
        assert!(check_service_name("demo", "we b").is_err());
        assert!(check_service_name("demo", &"s".repeat(60)).is_err(), "container name would be too long");
    }

    #[test]
    fn start_order_puts_dependencies_first_and_is_deterministic() {
        let p = project(
            "services:\n  web: { image: a, needs: { api: started, cache: started } }\n  api: { image: b, needs: { db: healthy } }\n  db: { image: c, health: { cmd: ok } }\n  cache: { image: d }\n  zeta: { image: e }\n",
        );
        let order = start_order(&p).unwrap();
        assert_eq!(order, ["cache", "db", "api", "web", "zeta"]);
        let pos = |s: &str| order.iter().position(|x| x == s).unwrap();
        for (svc, deps) in [("web", vec!["api", "cache"]), ("api", vec!["db"])] {
            for d in deps {
                assert!(pos(d) < pos(svc), "{d} before {svc}");
            }
        }
        assert_eq!(start_order(&p).unwrap(), order);
    }

    #[test]
    fn published_ports_are_tcp_only_sorted_and_unique() {
        let p = project(
            "services:\n  web: { image: a, ports: ['8080:80', '5353:53/udp'] }\n  api: { image: b, ports: ['3000:3000'] }\n",
        );
        let ports = published_ports(&p).unwrap();
        assert_eq!(ports.iter().map(|m| (m.host, m.container)).collect::<Vec<_>>(), [(3000, 3000), (8080, 80)]);

        let dup = project("services:\n  a: { image: x, ports: ['80:80'] }\n  b: { image: y, ports: ['80:8080'] }\n");
        let err = published_ports(&dup).unwrap_err().to_string();
        assert!(err.contains("host port 80 is published by both a and b"), "{err}");
        let same = project("services:\n  a: { image: x, ports: ['80:80', '80:81'] }\n");
        assert!(published_ports(&same).is_err());
        // the same number over TCP and UDP is two different things
        let mixed = project("services:\n  a: { image: x, ports: ['53:53', '53:53/udp'] }\n");
        assert_eq!(published_ports(&mixed).unwrap().len(), 1);
    }

    #[test]
    fn ports_are_formatted_for_listings() {
        let p = project("services:\n  a: { image: x, ports: ['8080:80', '5353:53/udp'] }\n");
        assert_eq!(format_ports(&p.services["a"].ports), "127.0.0.1:8080->80/tcp, 5353->53/udp (not published)");
        assert_eq!(format_ports(&[]), "");
    }

    fn vol(spec: &str) -> yarc_config::VolumeMount {
        project(&format!("services:\n  a: {{ image: x, volumes: ['{spec}'] }}\n")).services["a"].volumes[0].clone()
    }

    #[test]
    fn volume_sources_are_resolved_against_the_project_the_home_and_the_store() {
        let (base, vols, home) = (Path::new("/work/app"), Path::new("/store/volumes"), Path::new("/home/me"));
        let r = |s: &str| resolve_volume(base, vols, "demo", Some(home), &vol(s)).unwrap();
        assert_eq!(r("./data:/d").source, Path::new("/work/app/data"));
        assert_eq!(r("../shared:/d").source, Path::new("/work/shared"));
        assert_eq!(r("./a/../b/./c:/d").source, Path::new("/work/app/b/c"));
        assert_eq!(r("/abs/path:/d").source, Path::new("/abs/path"));
        assert_eq!(r("~/cache:/d").source, Path::new("/home/me/cache"));
        assert_eq!(r("~:/d").source, Path::new("/home/me"));

        let named = r("pgdata:/var/lib/pg:ro");
        assert_eq!(named.source, Path::new("/store/volumes/demo_pgdata"));
        assert_eq!((named.target.as_str(), named.read_only), ("/var/lib/pg", true));
        assert!(!r("./data:/d").read_only);
    }

    #[test]
    fn bad_volumes_are_rejected_with_the_volume_in_the_message() {
        let (base, vols) = (Path::new("/work"), Path::new("/store/volumes"));
        let err =
            |s: &str, home: Option<&Path>| resolve_volume(base, vols, "demo", home, &vol(s)).unwrap_err().to_string();
        assert!(err("./d:/", None).contains("./d:/"), "root as a target");
        assert!(err("./d:/a/../b", None).contains("without '..'"));
        assert!(err("~/x:/d", None).contains("HOME"));
        assert!(err("we ird!:/d", None).contains("volume name"));
    }

    #[test]
    fn volumes_are_formatted_as_written_and_named_ones_are_listed_once() {
        assert_eq!(format_volume(&vol("./data:/d")), "./data:/d");
        assert_eq!(format_volume(&vol("pg:/d:ro")), "pg:/d:ro");

        let p = project(
            "services:\n  a: { image: x, volumes: ['store:/s', './host:/h', 'cache:/c'] }\n  b: { image: x, volumes: ['store:/s2:ro'] }\n",
        );
        let loaded = Loaded {
            name: "demo".into(),
            file: PathBuf::from("/w/yarc.yaml"),
            order: start_order(&p).unwrap(),
            project: p,
            skipped: vec![],
        };
        let dirs = named_volume_dirs(Path::new("/root"), &loaded);
        assert_eq!(dirs, [Path::new("/root/volumes/demo_cache"), Path::new("/root/volumes/demo_store")]);
    }

    #[test]
    fn clean_path_is_lexical() {
        assert_eq!(clean_path(Path::new("/a/./b/../c")), Path::new("/a/c"));
        assert_eq!(clean_path(Path::new("/../x")), Path::new("/x"));
        assert_eq!(clean_path(Path::new("a/../../b")), Path::new("../b"));
    }

    #[test]
    fn independent_services_are_alphabetical() {
        let p = project("services:\n  b: { image: x }\n  a: { image: x }\n  c: { image: x }\n");
        assert_eq!(start_order(&p).unwrap(), ["a", "b", "c"]);
    }

    #[test]
    fn the_hash_covers_the_definition_but_not_needs() {
        let base = project("services:\n  db: { image: pg }\n  app: { image: x, env: { A: '1' }, ports: ['80:80'] }\n");
        let app = &base.services["app"];
        let h = service_hash(app);
        assert_eq!(h, service_hash(&app.clone()));
        assert_eq!(h.len(), 64);

        let changed = |yaml: &str| service_hash(&project(yaml).services["app"]);
        assert_ne!(h, changed("services:\n  app: { image: y, env: { A: '1' }, ports: ['80:80'] }\n"), "image");
        assert_ne!(h, changed("services:\n  app: { image: x, env: { A: '2' }, ports: ['80:80'] }\n"), "env");
        assert_ne!(
            h,
            changed("services:\n  app: { image: x, cmd: [true], env: { A: '1' }, ports: ['80:80'] }\n"),
            "cmd"
        );
        assert_ne!(
            h,
            changed("services:\n  app: { image: x, user: root, env: { A: '1' }, ports: ['80:80'] }\n"),
            "user"
        );
        assert_ne!(h, changed("services:\n  app: { image: x, env: { A: '1' }, ports: ['81:80'] }\n"), "ports");
        assert_ne!(
            h,
            changed("services:\n  app: { image: x, env: { A: '1' }, ports: ['80:80'], volumes: ['v:/d'] }\n"),
            "volumes"
        );
        assert_ne!(
            h,
            changed("services:\n  app: { image: x, env: { A: '1' }, ports: ['80:80'], health: { cmd: 'true' } }\n"),
            "health"
        );
        let with_needs = changed("services:\n  db: { image: pg }\n  app: { image: x, env: { A: '1' }, ports: ['80:80'], needs: { db: started } }\n");
        assert_eq!(h, with_needs, "needs only affects ordering");
    }

    fn spec(id: &str, service: Option<&str>) -> ContainerSpec {
        let p = yarc_runtime::Process { args: vec!["x".into()], env: vec![], working_dir: "/".into(), user: None };
        let mut s = ContainerSpec::new(id, "img", "/r".into(), p, yarc_runtime::Network::Host);
        if let Some(svc) = service {
            s.labels.insert(LABEL_SERVICE.into(), svc.into());
        }
        s
    }

    #[test]
    fn containers_sort_by_service_order_with_orphans_at_the_other_end() {
        let order: Vec<String> = ["db", "api", "web"].iter().map(|s| s.to_string()).collect();
        let ids = |specs: &[ContainerSpec]| specs.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
        let mut specs = vec![
            spec("w", Some("web")),
            spec("o", Some("old")),
            spec("d", Some("db")),
            spec("a", Some("api")),
            spec("u", None),
        ];

        sort_by_service_order(&mut specs, &order, false);
        assert_eq!(ids(&specs), ["d", "a", "w", "o", "u"], "start order, orphans last");
        sort_by_service_order(&mut specs, &order, true);
        assert_eq!(ids(&specs), ["u", "o", "w", "a", "d"], "shutdown: orphans first, dependents before dependencies");
    }

    #[test]
    fn labels_identify_project_service_and_definition() {
        let l = labels("demo", "web", "abc");
        assert_eq!(l[LABEL_PROJECT], "demo");
        assert_eq!(l[LABEL_SERVICE], "web");
        assert_eq!(l[LABEL_HASH], "abc");
    }

    #[test]
    fn loading_checks_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("yarc.yaml");
        let opts = Opts { file: file.clone(), name: Some("demo".into()), profiles: vec![], env_files: vec![] };
        assert!(load(&opts).unwrap_err().to_string().contains("cannot read"));

        std::fs::write(&file, "services: {}\n").unwrap();
        assert!(format!("{:#}", load(&opts).unwrap_err()).contains("no services"));
        std::fs::write(&file, "services:\n  a: { image: x, needs: { b: started } }\n").unwrap();
        assert!(format!("{:#}", load(&opts).unwrap_err()).contains("unknown"), "validation errors are shown");
        std::fs::write(&file, "services:\n  'bad name': { image: x }\n").unwrap();
        assert!(load(&opts).unwrap_err().to_string().contains("invalid service name"));

        std::fs::write(&file, "services:\n  db: { image: pg }\n  web: { image: nginx, needs: { db: started } }\n")
            .unwrap();
        let loaded = load(&opts).unwrap();
        assert_eq!(
            (loaded.name.as_str(), loaded.order.as_slice()),
            ("demo", &["db".to_string(), "web".to_string()][..])
        );
    }
}
