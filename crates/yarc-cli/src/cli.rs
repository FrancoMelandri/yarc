//! Command-line definition.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "yarc", version, about = "Yet Another Runtime Container")]
pub struct Cli {
    /// Where images and state are stored [default: per-user data directory]
    #[arg(long, global = true, env = "YARC_ROOT", value_name = "DIR")]
    pub root: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download an image from a registry into the local store
    Pull(PullArgs),
    /// Work with images in the local store
    #[command(subcommand)]
    Image(ImageCommand),
}

#[derive(Debug, Args)]
pub struct PullArgs {
    /// Image reference, e.g. alpine, postgres:16, ghcr.io/org/app:1.2
    pub image: String,

    /// Parallel layer downloads
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=64))]
    pub jobs: u16,

    /// Platform to pull, as OS/ARCH [default: linux/<this machine's arch>]
    #[arg(long, value_name = "OS/ARCH")]
    pub platform: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ImageCommand {
    /// Extract a pulled image into a directory (Linux only)
    Unpack(UnpackArgs),
}

#[derive(Debug, Args)]
pub struct UnpackArgs {
    /// Image reference that was pulled before
    pub image: String,
    /// Destination directory (must not exist or be empty)
    pub dir: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("yarc").chain(args.iter().copied()))
    }

    #[test]
    fn pull_defaults() {
        let cli = parse(&["pull", "alpine"]).unwrap();
        let Command::Pull(a) = cli.command else { panic!() };
        assert_eq!(a.image, "alpine");
        assert_eq!(a.jobs, 4);
        assert_eq!(a.platform, None);
    }

    #[test]
    fn pull_options_and_global_root() {
        let cli = parse(&["pull", "--jobs", "8", "--platform", "linux/arm64", "--root", "/tmp/y", "nginx:1"]).unwrap();
        assert_eq!(cli.root, Some(PathBuf::from("/tmp/y")));
        let Command::Pull(a) = cli.command else { panic!() };
        assert_eq!((a.jobs, a.platform.as_deref()), (8, Some("linux/arm64")));
    }

    #[test]
    fn image_unpack() {
        let cli = parse(&["image", "unpack", "alpine", "./rootfs"]).unwrap();
        let Command::Image(ImageCommand::Unpack(a)) = cli.command else { panic!() };
        assert_eq!((a.image.as_str(), a.dir), ("alpine", PathBuf::from("./rootfs")));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["pull"]).is_err());
        assert!(parse(&["pull", "x", "--jobs", "0"]).is_err());
        assert!(parse(&["image", "unpack", "alpine"]).is_err());
        assert!(parse(&["frobnicate"]).is_err());
    }
}
