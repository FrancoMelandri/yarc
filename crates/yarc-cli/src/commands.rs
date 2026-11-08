//! Command implementations.
//!
//! For now these call `yarc-oci` directly; once the agent exists (M3) they will
//! go through it.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use yarc_oci::{BlobStore, Platform, PullReport, Reference, RegistryClient, UreqTransport};

use crate::cli::{PullArgs, UnpackArgs};
#[cfg(unix)]
use crate::cli::{CreateArgs, RmArgs};

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

/// Creates a container record and its rootfs; prints the id. Pulls the image
/// first when it is not in the local store.
#[cfg(unix)]
pub fn create(root: &Path, args: CreateArgs) -> Result<()> {
    use yarc_runtime::{resolve_process, ContainerSpec, ContainerStore, ImageDefaults, Overrides};

    let reference: Reference = args.image.parse()?;
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
    let overrides = Overrides {
        entrypoint: args.entrypoint.as_ref().map(|e| if e.is_empty() { vec![] } else { vec![e.clone()] }),
        cmd: args.cmd,
        env: args.env,
        working_dir: args.workdir,
        user: args.user,
    };
    // Fail on a bad command line before touching the disk.
    let process = resolve_process(&defaults, &overrides)?;

    let containers = ContainerStore::open(root)?;
    let id = containers.allocate(args.name.as_deref())?;
    let rootfs = containers.rootfs_path(&id);
    let built = (|| -> Result<()> {
        yarc_oci::unpack_image(&store, &reference, &rootfs)?;
        let spec = ContainerSpec::new(&id, &args.image, rootfs.clone(), process, args.network);
        containers.save(&spec)?;
        Ok(())
    })();
    if let Err(err) = built {
        let _ = containers.remove(&id); // don't leave a half-built container behind
        return Err(err);
    }
    println!("{id}");
    Ok(())
}

#[cfg(not(unix))]
pub fn create(_root: &Path, _args: crate::cli::CreateArgs) -> Result<()> {
    anyhow::bail!("creating a container needs Linux; run yarc inside WSL")
}

pub fn container_ls(root: &Path) -> Result<()> {
    let containers = yarc_runtime::ContainerStore::open(root)?;
    println!("{:<14} {:<24} COMMAND", "CONTAINER ID", "IMAGE");
    for c in containers.list()? {
        println!("{:<14} {:<24} {}", c.id, c.image, c.args.join(" "));
    }
    Ok(())
}

/// Runs a created container in the foreground; the result is yarc's exit code.
pub fn container_start(root: &Path, args: crate::cli::StartArgs) -> Result<u8> {
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
    let code = yarc_runtime::run(&spec, &yarc_runtime::RunOptions { debug: args.debug })?;
    Ok((code & 0xff) as u8)
}

#[cfg(unix)]
pub fn container_rm(root: &Path, args: RmArgs) -> Result<()> {
    let containers = yarc_runtime::ContainerStore::open(root)?;
    for id in &args.ids {
        containers.remove(id)?;
        println!("{id}");
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn container_rm(_root: &Path, _args: crate::cli::RmArgs) -> Result<()> {
    anyhow::bail!("run yarc inside WSL")
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
        Some((os, arch)) if !os.is_empty() && !arch.is_empty() && !arch.contains('/') => {
            Ok(Platform::new(os, arch))
        }
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
