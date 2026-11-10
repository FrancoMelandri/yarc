//! Command implementations.
//!
//! For now these call `yarc-oci` directly; once the agent exists (M3) they will
//! go through it.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use yarc_oci::{BlobStore, Platform, PullReport, Reference, RegistryClient, UreqTransport};

#[cfg(unix)]
use crate::cli::CreateArgs;
use crate::cli::{PullArgs, UnpackArgs};

fn open_store(root: &Path) -> Result<BlobStore> {
    BlobStore::open(root).with_context(|| format!("cannot open store at {}", root.display()))
}

/// Resolves and downloads an image; returns the manifest digest and the report.
fn fetch(store: &BlobStore, reference: &Reference, platform: &Platform, jobs: usize) -> Result<(String, PullReport)> {
    let client = RegistryClient::new(UreqTransport::new());
    eprintln!("Resolving {reference} ({}/{})", platform.os, platform.architecture);
    let image = client.resolve(reference, platform)?;
    eprintln!("Pulling {} layer(s)", image.manifest.layers.len());
    let report = yarc_oci::pull(&client, store, &image, jobs)?;
    Ok((image.manifest_digest.to_string(), report))
}

pub fn pull(root: &Path, args: PullArgs) -> Result<()> {
    let reference: Reference = args.image.parse()?;
    let platform = match &args.platform {
        Some(p) => parse_platform(p)?,
        None => Platform::host(),
    };
    let store = open_store(root)?;
    let (digest, report) = fetch(&store, &reference, &platform, usize::from(args.jobs))?;

    println!("{reference}");
    println!("  digest: {digest}");
    println!(
        "  layers: {} downloaded ({}), {} already present",
        report.downloaded,
        human_bytes(report.bytes_downloaded),
        report.skipped
    );
    Ok(())
}

/// Name as users type it: without the Docker Hub host and `library/` prefix.
fn short_name(reference: &Reference) -> String {
    let full = reference.to_string();
    let name = full.strip_prefix("registry-1.docker.io/").unwrap_or(&full);
    name.strip_prefix("library/").unwrap_or(name).to_string()
}

/// `2026-11-03T13:47:20.28Z` -> `2026-11-03 13:47:20`; anything odd is shown as is.
fn short_time(created: Option<&str>) -> String {
    match created {
        Some(t) if t.len() >= 19 && t.as_bytes()[10] == b'T' => format!("{} {}", &t[..10], &t[11..19]),
        Some(t) => t.to_string(),
        None => "-".into(),
    }
}

/// `yarc image ls`
pub fn image_ls(root: &Path) -> Result<()> {
    let store = open_store(root)?;
    println!("{:<44} {:<14} {:>10}  CREATED", "IMAGE", "IMAGE ID", "SIZE");
    for (reference, digest) in store.list_refs()? {
        let id = &digest.hex()[..12];
        match store.load_image(&reference) {
            Ok(img) => {
                let size: u64 = img.manifest.layers.iter().map(|l| l.size).sum();
                println!(
                    "{:<44} {:<14} {:>10}  {}",
                    short_name(&reference),
                    id,
                    human_bytes(size),
                    short_time(img.config.created.as_deref())
                );
            }
            Err(e) => println!("{:<44} {:<14} {:>10}  (unreadable: {e})", short_name(&reference), id, "-"),
        }
    }
    Ok(())
}

/// `yarc image rm`: removes every image it can; fails if any was unknown or could not be removed.
pub fn image_rm(root: &Path, args: crate::cli::ImageRmArgs) -> Result<()> {
    let store = open_store(root)?;
    let mut failed = 0;
    for name in &args.images {
        let result = name.parse::<Reference>().map_err(anyhow::Error::from).and_then(|reference| {
            match store.remove_image(&reference)? {
                Some(r) => {
                    println!(
                        "Removed {} ({}; freed {} in {} blob(s))",
                        short_name(&reference),
                        &r.digest.hex()[..12],
                        human_bytes(r.bytes_freed),
                        r.blobs_removed
                    );
                    Ok(())
                }
                None => Err(anyhow!("image {name} is not in the local store")),
            }
        });
        if let Err(e) = result {
            eprintln!("error: {e:#}");
            failed += 1;
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} image(s) could not be removed");
    }
    Ok(())
}

/// `yarc load`: import an OCI layout into the store; works offline and on any OS.
pub fn load(root: &Path, args: crate::cli::LoadArgs) -> Result<()> {
    let store = open_store(root)?;
    let report =
        yarc_oci::load_path(&store, &args.file).with_context(|| format!("cannot load {}", args.file.display()))?;
    for image in &report.images {
        println!("Loaded {}", image.reference);
        println!("  digest: {}", image.manifest_digest);
    }
    for digest in &report.unnamed {
        eprintln!("warning: skipped {digest}: it has no image name (annotation io.containerd.image.name)");
    }
    println!(
        "{} image(s); {} blob(s) added ({}), {} already present",
        report.images.len(),
        report.blobs_added,
        human_bytes(report.bytes_added),
        report.blobs_present
    );
    Ok(())
}

/// `yarc create`: prints the id of the new container.
pub fn create(root: &Path, args: crate::cli::CreateArgs) -> Result<()> {
    println!("{}", create_container(root, args)?);
    Ok(())
}

/// `yarc run`: create, start in the foreground, and with `--rm` delete the
/// container afterwards (whether or not it started). Returns yarc's exit code.
pub fn run(root: &Path, args: crate::cli::RunArgs) -> Result<u8> {
    check_restart_options(&args)?;
    let id = create_container(root, args.create)?;
    if args.detach {
        // The shim removes the container itself when `--rm` is given and it exits.
        let started = crate::detached::start(root, &id, args.debug, args.rm);
        if started.is_err() && args.rm {
            let _ = yarc_runtime::ContainerStore::open(root).and_then(|c| c.remove(&id));
        }
        started?;
        println!("{id}");
        return Ok(0);
    }
    let started = container_start(root, crate::cli::StartArgs { id: id.clone(), detach: false, debug: args.debug });
    if args.rm {
        // A failed cleanup must not hide the container's own result.
        if let Err(err) = yarc_runtime::ContainerStore::open(root).and_then(|c| c.remove(&id)) {
            eprintln!("warning: could not remove container {id}: {err}");
        }
    }
    started
}

/// A restart policy is carried out by the detached supervisor, and a container that removes
/// itself on exit cannot be restarted.
fn check_restart_options(args: &crate::cli::RunArgs) -> Result<()> {
    if args.create.restart.is_no() {
        return Ok(());
    }
    if !args.detach {
        anyhow::bail!("--restart needs --detach: only a detached container is supervised across restarts");
    }
    if args.rm {
        anyhow::bail!("--restart cannot be combined with --rm");
    }
    Ok(())
}

/// Everything needed to create one container, whoever asks (`create`, `run`, `up`).
#[cfg(unix)]
pub struct NewContainer {
    pub image: String,
    pub name: Option<String>,
    pub overrides: yarc_runtime::Overrides,
    pub network: yarc_runtime::Network,
    pub labels: std::collections::BTreeMap<String, String>,
    pub health: Option<yarc_runtime::HealthSpec>,
    /// A project's network directory and shared hosts file; makes the container
    /// join the project's network (overriding `network`).
    pub project_net: Option<(std::path::PathBuf, std::path::PathBuf)>,
    pub volumes: Vec<yarc_runtime::VolumeSpec>,
    pub restart: yarc_runtime::RestartPolicy,
}

#[cfg(unix)]
fn create_container(root: &Path, args: CreateArgs) -> Result<String> {
    create_new(
        root,
        NewContainer {
            image: args.image,
            name: args.name,
            overrides: yarc_runtime::Overrides {
                entrypoint: args.entrypoint.map(|e| if e.is_empty() { vec![] } else { vec![e] }),
                cmd: args.cmd,
                env: args.env,
                working_dir: args.workdir,
                user: args.user,
            },
            network: args.network,
            labels: Default::default(),
            health: None,
            project_net: None,
            volumes: Vec::new(),
            restart: args.restart,
        },
    )
}

/// Creates a container record and its rootfs and returns its id. Pulls the
/// image first when it is not in the local store.
#[cfg(unix)]
pub fn create_new(root: &Path, new: NewContainer) -> Result<String> {
    use yarc_runtime::{resolve_process, ContainerSpec, ContainerStore, ImageDefaults};

    let reference: Reference = new.image.parse()?;
    let store = open_store(root)?;
    if store.get_ref(&reference)?.is_none() {
        eprintln!("{reference} is not in the local store, pulling it");
        fetch(&store, &reference, &Platform::host(), 4)?;
    }
    let image = store.load_image(&reference)?;
    let cfg = &image.config.config;
    let defaults = ImageDefaults {
        entrypoint: cfg.entrypoint.clone().unwrap_or_default(),
        cmd: cfg.cmd.clone().unwrap_or_default(),
        env: cfg.env.clone().unwrap_or_default(),
        working_dir: cfg.working_dir.clone(),
        user: cfg.user.clone(),
    };
    // Fail on a bad command line before touching the disk.
    let process = resolve_process(&defaults, &new.overrides)?;

    let containers = ContainerStore::open(root)?;
    let id = containers.allocate(new.name.as_deref())?;
    let rootfs = containers.rootfs_path(&id);
    let built = (|| -> Result<()> {
        yarc_oci::unpack_image(&store, &reference, &rootfs)?;
        let mut spec = ContainerSpec::new(&id, &new.image, rootfs.clone(), process, new.network)
            .with_labels(new.labels)
            .with_health(new.health)
            .with_volumes(new.volumes)
            .with_restart(new.restart);
        if let Some((dir, hosts)) = new.project_net {
            spec = spec.in_project_network(dir, hosts);
        }
        containers.save(&spec)?;
        Ok(())
    })();
    if let Err(err) = built {
        let _ = containers.remove(&id); // don't leave a half-built container behind
        return Err(err);
    }
    Ok(id)
}

#[cfg(not(unix))]
fn create_container(_root: &Path, _args: crate::cli::CreateArgs) -> Result<String> {
    anyhow::bail!("creating a container needs Linux; run yarc inside WSL")
}

/// `container ls` (everything) and `ps` (running only unless `-a`).
pub fn container_ls(root: &Path, all: bool) -> Result<()> {
    let containers = yarc_runtime::ContainerStore::open(root)?;
    println!("{:<14} {:<24} {:<12} COMMAND", "CONTAINER ID", "IMAGE", "STATUS");
    for c in containers.list()? {
        let status = containers.status_text(&c.id)?;
        if all || status.starts_with("Running") || status.starts_with("Restarting") {
            println!("{:<14} {:<24} {:<12} {}", c.id, c.image, status, c.args.join(" "));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn container_start(_root: &Path, _args: crate::cli::StartArgs) -> Result<u8> {
    anyhow::bail!("running containers needs Linux; run yarc inside WSL")
}

/// Runs a created container in the foreground (recording its state and log);
/// the result is yarc's exit code.
#[cfg(target_os = "linux")]
pub fn container_start(root: &Path, args: crate::cli::StartArgs) -> Result<u8> {
    if args.detach {
        crate::detached::start(root, &args.id, args.debug, false)?;
        println!("{}", args.id);
        return Ok(0);
    }
    let containers = yarc_runtime::ContainerStore::open(root)?;
    let spec = containers.load(&args.id)?;
    if !spec.rootfs.is_dir() {
        anyhow::bail!("the root filesystem of container {} is missing ({})", spec.id, spec.rootfs.display());
    }
    if args.debug {
        eprintln!("yarc: setup plan for {}:", spec.id);
        for op in yarc_runtime::mount_plan(&spec) {
            eprintln!("  {op:?}");
        }
    }
    if !spec.resources.is_unset() && !yarc_runtime::in_scope() {
        return restart_in_scope(root, &spec, args.debug);
    }
    let launch = yarc_runtime::Launch { debug: args.debug, detached: false };
    let code = yarc_runtime::run_recorded(&containers, &spec, &launch)?;
    Ok((code & 0xff) as u8)
}

/// A foreground container with limits runs in a systemd scope that carries them: this process
/// is replaced by the same `container start` inside the scope (same terminal, same exit code).
#[cfg(target_os = "linux")]
fn restart_in_scope(root: &Path, spec: &yarc_runtime::ContainerSpec, debug: bool) -> Result<u8> {
    use anyhow::Context;
    use std::os::unix::process::CommandExt;

    yarc_runtime::check_host(&spec.resources, yarc_runtime::host_cpus())?;
    yarc_runtime::require_scopes()?;
    let exe = std::env::current_exe().context("cannot locate the yarc executable")?;
    let mut cmd = yarc_runtime::scope_command(&spec.resources, &exe);
    cmd.arg("--root").arg(root).args(["container", "start"]).arg(&spec.id);
    if debug {
        cmd.arg("--debug");
    }
    Err(anyhow::Error::new(cmd.exec()).context("cannot enter the limits scope"))
}

/// Prints a container's recorded output: stdout lines to stdout, stderr lines
/// to stderr, like `docker logs`.
pub fn container_logs(root: &Path, args: crate::cli::LogsArgs) -> Result<()> {
    use yarc_runtime::Stream;

    let containers = yarc_runtime::ContainerStore::open(root)?;
    containers.load(&args.id)?; // "no such container" for unknown ids
    let path = containers.log_path(&args.id);
    let print = |entries: &[yarc_runtime::LogEntry]| {
        for entry in entries {
            let line = if args.timestamps {
                format!("{} {}", yarc_runtime::format_timestamp(entry.ts), entry.line)
            } else {
                entry.line.clone()
            };
            match entry.stream {
                Stream::Stdout => println!("{line}"),
                Stream::Stderr => eprintln!("{line}"),
            }
        }
    };

    let (entries, mut offset) = yarc_runtime::read_log_from(&path, 0)?;
    print(&entries[args.tail.map_or(0, |n| entries.len().saturating_sub(n))..]);
    if !args.follow {
        return Ok(());
    }
    loop {
        // Look at the lock *before* reading: whatever the container wrote before
        // it exited is then guaranteed to be in this last read.
        let running = containers.is_supervised(&args.id)?;
        let (entries, next) = yarc_runtime::read_log_from(&path, offset)?;
        offset = next;
        print(&entries);
        if !running {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(unix)]
pub fn unpack(root: &Path, args: UnpackArgs) -> Result<()> {
    let reference: Reference = args.image.parse()?;
    let store = open_store(root)?;

    let report = yarc_oci::unpack_image(&store, &reference, &args.dir)?;

    println!("{reference} -> {}", args.dir.display());
    println!(
        "  {} files, {} dirs, {} symlinks, {} hardlinks, {} whiteouts applied",
        report.files, report.dirs, report.symlinks, report.hardlinks, report.whiteouts
    );
    if report.skipped_special > 0 {
        println!("  {} device node(s)/fifo(s) skipped", report.skipped_special);
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn unpack(_root: &Path, _args: UnpackArgs) -> Result<()> {
    anyhow::bail!("unpacking an image needs Linux file semantics; run yarc inside WSL")
}

fn parse_platform(s: &str) -> Result<Platform> {
    match s.split_once('/') {
        Some((os, arch)) if !os.is_empty() && !arch.is_empty() && !arch.contains('/') => Ok(Platform::new(os, arch)),
        _ => Err(anyhow!("invalid platform '{s}', expected OS/ARCH, e.g. linux/amd64")),
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_parsing() {
        assert_eq!(parse_platform("linux/arm64").unwrap(), Platform::new("linux", "arm64"));
        for bad in ["linux", "/arm64", "linux/", "a/b/c", ""] {
            assert!(parse_platform(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn human_readable_sizes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(3_630_321), "3.5 MiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}

#[cfg(test)]
mod image_list_tests {
    use super::*;

    #[test]
    fn names_are_shortened_like_users_type_them() {
        let s = |x: &str| short_name(&x.parse().unwrap());
        assert_eq!(s("alpine:3.20"), "alpine:3.20");
        assert_eq!(s("dotnet/aspire-dashboard:9.5"), "dotnet/aspire-dashboard:9.5");
        assert_eq!(s("ghcr.io/org/app:1"), "ghcr.io/org/app:1");
    }

    #[test]
    fn times_are_shortened() {
        assert_eq!(short_time(Some("2026-11-03T13:47:20.282607838Z")), "2026-11-03 13:47:20");
        assert_eq!(short_time(Some("weird")), "weird");
        assert_eq!(short_time(None), "-");
    }
}
