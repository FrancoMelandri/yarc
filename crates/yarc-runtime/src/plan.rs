//! What the container's filesystem setup consists of, as plain data.
//!
//! Building the plan is pure (and tested everywhere); executing it needs Linux
//! and lives in `linux.rs`. Targets are paths relative to the container's
//! rootfs; sources are paths on the host.

use std::path::PathBuf;

use crate::error::{Result, RuntimeError};
use crate::spec::{ContainerSpec, Network};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MountOpts {
    pub nosuid: bool,
    pub nodev: bool,
    pub noexec: bool,
    pub readonly: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindKind {
    File,
    Dir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupOp {
    /// Mount a fresh filesystem (`proc`, `tmpfs`, ...) at `target`.
    Fs { fstype: &'static str, target: String, data: String, opts: MountOpts, optional: bool },
    /// Bind-mount a host path at `target`.
    Bind { source: PathBuf, target: String, kind: BindKind, recursive: bool, opts: MountOpts, optional: bool },
    /// Create a symlink at `path` pointing to `points_to`.
    Symlink { path: String, points_to: String },
    /// Write a generated file (replacing whatever the image has there).
    WriteFile { path: String, contents: String },
    /// Copy a host file into the rootfs (best effort).
    CopyHost { source: PathBuf, path: String },
}

impl SetupOp {
    /// Optional operations may fail without aborting the start (e.g. `/sys` in
    /// restricted environments).
    pub fn optional(&self) -> bool {
        match self {
            SetupOp::Fs { optional, .. } | SetupOp::Bind { optional, .. } => *optional,
            SetupOp::CopyHost { .. } => true,
            SetupOp::Symlink { .. } | SetupOp::WriteFile { .. } => false,
        }
    }
}

const DEV_NODES: [&str; 6] = ["null", "zero", "full", "random", "urandom", "tty"];

/// The ordered setup for `/proc`, `/dev`, `/sys` and the generated `/etc` files.
pub fn mount_plan(spec: &ContainerSpec) -> Vec<SetupOp> {
    let locked = MountOpts { nosuid: true, nodev: true, noexec: true, readonly: false };
    let mut plan = vec![
        SetupOp::Fs { fstype: "proc", target: "proc".into(), data: String::new(), opts: locked, optional: false },
        SetupOp::Fs {
            fstype: "tmpfs",
            target: "dev".into(),
            data: "mode=755,size=65536k".into(),
            opts: MountOpts { nosuid: true, ..Default::default() },
            optional: false,
        },
    ];
    for node in DEV_NODES {
        plan.push(SetupOp::Bind {
            source: PathBuf::from("/dev").join(node),
            target: format!("dev/{node}"),
            kind: BindKind::File,
            recursive: false,
            opts: MountOpts::default(),
            // A host without /dev/tty (no controlling terminal setups) is fine.
            optional: node == "tty",
        });
    }
    plan.push(SetupOp::Fs {
        fstype: "devpts",
        target: "dev/pts".into(),
        data: "newinstance,ptmxmode=0666,mode=0620".into(),
        opts: MountOpts { nosuid: true, noexec: true, ..Default::default() },
        optional: true,
    });
    for (path, points_to) in [
        ("dev/ptmx", "pts/ptmx"),
        ("dev/fd", "/proc/self/fd"),
        ("dev/stdin", "/proc/self/fd/0"),
        ("dev/stdout", "/proc/self/fd/1"),
        ("dev/stderr", "/proc/self/fd/2"),
    ] {
        plan.push(SetupOp::Symlink { path: path.into(), points_to: points_to.into() });
    }
    plan.push(SetupOp::Fs {
        fstype: "tmpfs",
        target: "dev/shm".into(),
        data: "mode=1777,size=65536k".into(),
        opts: MountOpts { nosuid: true, nodev: true, noexec: true, readonly: false },
        optional: false,
    });

    // sysfs can only be mounted by a user namespace that owns the network
    // namespace; otherwise fall back to a read-only view of the host's /sys.
    let ro_locked = MountOpts { readonly: true, ..locked };
    plan.push(match spec.network {
        Network::None => SetupOp::Fs { fstype: "sysfs", target: "sys".into(), data: String::new(), opts: ro_locked, optional: true },
        Network::Host => SetupOp::Bind {
            source: PathBuf::from("/sys"),
            target: "sys".into(),
            kind: BindKind::Dir,
            recursive: true,
            opts: ro_locked,
            optional: true,
        },
    });

    plan.push(SetupOp::WriteFile { path: "etc/hostname".into(), contents: format!("{}\n", spec.hostname) });
    plan.push(SetupOp::WriteFile {
        path: "etc/hosts".into(),
        contents: format!(
            "127.0.0.1\tlocalhost {}\n::1\tlocalhost ip6-localhost ip6-loopback\n",
            spec.hostname
        ),
    });
    if spec.network == Network::Host {
        plan.push(SetupOp::CopyHost { source: PathBuf::from("/etc/resolv.conf"), path: "etc/resolv.conf".into() });
    }
    plan
}

/// Rootless mode maps a single uid/gid (the invoking user) to 0 in the
/// container, so only root can be requested for now.
pub fn check_user(user: Option<&str>) -> Result<()> {
    let Some(user) = user else { return Ok(()) };
    let (uid, gid) = match user.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (user, None),
    };
    let is_root = |s: &str| s == "0" || s == "root";
    if is_root(uid) && gid.is_none_or(is_root) {
        Ok(())
    } else {
        Err(RuntimeError::Unsupported(format!(
            "user '{user}': rootless containers can only run as root (uid 0) for now"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Process;

    fn spec(network: Network) -> ContainerSpec {
        let p = Process { args: vec!["x".into()], env: vec![], working_dir: "/".into(), user: None };
        ContainerSpec::new("0123456789abcdef", "alpine", "/r".into(), p, network)
    }

    fn targets(plan: &[SetupOp]) -> Vec<String> {
        plan.iter()
            .map(|op| match op {
                SetupOp::Fs { target, .. } | SetupOp::Bind { target, .. } => target.clone(),
                SetupOp::Symlink { path, .. } | SetupOp::WriteFile { path, .. } | SetupOp::CopyHost { path, .. } => path.clone(),
            })
            .collect()
    }

    #[test]
    fn proc_and_dev_tmpfs_come_before_the_nodes_inside_dev() {
        let plan = mount_plan(&spec(Network::Host));
        let t = targets(&plan);
        let pos = |name: &str| t.iter().position(|x| x == name).unwrap_or_else(|| panic!("{name} missing"));
        assert!(pos("proc") < pos("dev/null"));
        assert!(pos("dev") < pos("dev/null"));
        assert!(pos("dev") < pos("dev/pts") && pos("dev") < pos("dev/shm") && pos("dev") < pos("dev/ptmx"));
        for node in DEV_NODES {
            assert!(t.contains(&format!("dev/{node}")), "{node}");
        }
    }

    #[test]
    fn device_nodes_are_bound_from_the_host() {
        let plan = mount_plan(&spec(Network::Host));
        assert!(plan.iter().any(|op| matches!(op,
            SetupOp::Bind { source, target, kind: BindKind::File, .. }
                if source == &PathBuf::from("/dev/urandom") && target == "dev/urandom")));
    }

    #[test]
    fn only_tty_among_the_devices_is_optional() {
        for op in mount_plan(&spec(Network::Host)) {
            if let SetupOp::Bind { target, optional, kind: BindKind::File, .. } = &op {
                assert_eq!(*optional, target == "dev/tty", "{target}");
            }
        }
    }

    #[test]
    fn sys_depends_on_the_network_mode() {
        let host = mount_plan(&spec(Network::Host));
        assert!(host.iter().any(|op| matches!(op, SetupOp::Bind { target, recursive: true, opts, .. } if target == "sys" && opts.readonly)));
        assert!(!host.iter().any(|op| matches!(op, SetupOp::Fs { fstype: "sysfs", .. })));
        let none = mount_plan(&spec(Network::None));
        assert!(none.iter().any(|op| matches!(op, SetupOp::Fs { fstype: "sysfs", opts, .. } if opts.readonly)));
    }

    #[test]
    fn resolv_conf_is_copied_only_with_host_network() {
        let has = |plan: &[SetupOp]| plan.iter().any(|op| matches!(op, SetupOp::CopyHost { path, .. } if path == "etc/resolv.conf"));
        assert!(has(&mount_plan(&spec(Network::Host))));
        assert!(!has(&mount_plan(&spec(Network::None))));
    }

    #[test]
    fn hostname_and_hosts_files_use_the_container_hostname() {
        let plan = mount_plan(&spec(Network::Host));
        let file = |name: &str| plan.iter().find_map(|op| match op {
            SetupOp::WriteFile { path, contents } if path == name => Some(contents.clone()),
            _ => None,
        });
        assert_eq!(file("etc/hostname").unwrap(), "0123456789ab\n");
        assert!(file("etc/hosts").unwrap().starts_with("127.0.0.1\tlocalhost 0123456789ab\n"));
    }

    #[test]
    fn required_and_optional_operations() {
        let plan = mount_plan(&spec(Network::Host));
        let get = |t: &str| plan.iter().find(|op| targets(std::slice::from_ref(op))[0] == t).unwrap().clone();
        assert!(!get("proc").optional());
        assert!(!get("dev").optional());
        assert!(get("sys").optional());
        assert!(get("dev/pts").optional());
        assert!(get("etc/resolv.conf").optional());
        assert!(!get("etc/hosts").optional());
    }

    #[test]
    fn only_root_is_accepted_as_user() {
        for ok in [None, Some("0"), Some("root"), Some("0:0"), Some("root:root"), Some("0:root")] {
            assert!(check_user(ok).is_ok(), "{ok:?}");
        }
        for bad in ["nginx", "1000", "1000:1000", "0:1000", "root:daemon"] {
            assert!(matches!(check_user(Some(bad)), Err(RuntimeError::Unsupported(_))), "{bad}");
        }
    }
}
