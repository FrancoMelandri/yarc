//! Command implementations.
//!
//! For now these call `yarc-oci` directly; once the agent exists (M3) they will
//! go through it.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use yarc_oci::{BlobStore, Platform, Reference, RegistryClient, UreqTransport};

use crate::cli::{PullArgs, UnpackArgs};

pub fn pull(root: &Path, args: PullArgs) -> Result<()> {
    let reference: Reference = args.image.parse()?;
    let platform = match &args.platform {
        Some(p) => parse_platform(p)?,
        None => Platform::host(),
    };
    let store = BlobStore::open(root).with_context(|| format!("cannot open store at {}", root.display()))?;
    let client = RegistryClient::new(UreqTransport::new());

    eprintln!("Resolving {reference} ({}/{})", platform.os, platform.architecture);
    let image = client.resolve(&reference, &platform)?;
    let layers = image.manifest.layers.len();
    eprintln!("Pulling {layers} layer(s)");
    let report = yarc_oci::pull(&client, &store, &image, usize::from(args.jobs))?;

    println!("{reference}");
    println!("  digest: {}", image.manifest_digest);
    println!(
        "  layers: {} downloaded ({}), {} already present",
        report.downloaded,
        human_bytes(report.bytes_downloaded),
        report.skipped
    );
    Ok(())
}

#[cfg(unix)]
pub fn unpack(root: &Path, args: UnpackArgs) -> Result<()> {
    let reference: Reference = args.image.parse()?;
    let store = BlobStore::open(root).with_context(|| format!("cannot open store at {}", root.display()))?;

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
