//! A project's shared network: on-disk state under `<root>/projects/<name>/`.
//!
//! All services of a project join one user + network namespace owned by a small
//! long-lived "holder" process (`netns.rs`), like the infra container of a
//! podman pod: they reach each other on `localhost`, and service names resolve
//! to it through a shared hosts file. Liveness is the holder's `flock` on
//! `net.lock`, exactly like a container's supervisor lock.
//!
//! ```text
//! <root>/projects/<name>/net.json    holder pid + published ports
//!                        net.lock    held by the holder while it runs
//!                        hosts       bind-mounted over /etc/hosts of every service
//!                        holder.log  the holder's stderr
//! ```

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::container::check_id;
use crate::error::{io_err, Result, RuntimeError};

/// A published port: `127.0.0.1:host` on the host forwards to
/// `127.0.0.1:container` inside the project network (TCP).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PortMap {
    pub host: u16,
    pub container: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetInfo {
    /// Host pid of the holder; `/proc/<pid>/ns/{user,net}` are what containers join.
    pub pid: u32,
    pub published: Vec<PortMap>,
}

#[derive(Debug, Clone)]
pub struct ProjectNet {
    dir: PathBuf,
}

impl ProjectNet {
    pub fn new(root: &Path, project: &str) -> Result<Self> {
        check_id(project).map_err(|_| RuntimeError::InvalidName(project.to_string()))?;
        Ok(ProjectNet { dir: root.join("projects").join(project) })
    }

    /// For a directory recorded in a container's spec.
    pub fn from_dir(dir: &Path) -> Self {
        ProjectNet { dir: dir.to_path_buf() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("net.lock")
    }

    pub fn info_path(&self) -> PathBuf {
        self.dir.join("net.json")
    }

    pub fn hosts_path(&self) -> PathBuf {
        self.dir.join("hosts")
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join("holder.log")
    }

    pub fn create_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.dir).map_err(io_err(format!("cannot create {}", self.dir.display())))
    }

    /// Takes the holder lock, or `None` if a holder is running.
    pub fn try_acquire(&self) -> Result<Option<File>> {
        self.create_dir()?;
        let path = self.lock_path();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(io_err(format!("cannot open {}", path.display())))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(RuntimeError::Io { context: format!("cannot lock {}", path.display()), source: e }),
        }
    }

    /// True while a holder process is running.
    pub fn is_alive(&self) -> Result<bool> {
        if !self.lock_path().exists() {
            return Ok(false);
        }
        Ok(self.try_acquire()?.is_none())
    }

    pub fn info(&self) -> Result<Option<NetInfo>> {
        let path = self.info_path();
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|source| RuntimeError::Json { path, source }),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(RuntimeError::Io { context: format!("cannot read {}", path.display()), source: e }),
        }
    }

    pub fn write_info(&self, info: &NetInfo) -> Result<()> {
        self.create_dir()?;
        let path = self.info_path();
        let json = serde_json::to_vec_pretty(info).map_err(|source| RuntimeError::Json { path: path.clone(), source })?;
        write_atomic(&path, &json)
    }

    /// The holder's pid, but only while it is really running (its lock is held),
    /// so a pid left over from a crashed holder is never used.
    pub fn alive_pid(&self) -> Result<Option<u32>> {
        if !self.is_alive()? {
            return Ok(None);
        }
        Ok(self.info()?.map(|i| i.pid))
    }

    /// Writes the shared hosts file: every name resolves to the loopback the
    /// services share.
    pub fn write_hosts(&self, names: &[String]) -> Result<()> {
        self.create_dir()?;
        let mut unique: Vec<&str> = names.iter().map(String::as_str).collect();
        unique.sort_unstable();
        unique.dedup();
        let content = format!(
            "127.0.0.1\tlocalhost {}\n::1\tlocalhost ip6-localhost ip6-loopback\n",
            unique.join(" ")
        );
        write_atomic(&self.hosts_path(), content.as_bytes())
    }

    /// Deletes the directory (the holder must be stopped first).
    pub fn remove(&self) -> Result<()> {
        match fs::remove_dir_all(&self.dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(RuntimeError::Io { context: format!("cannot remove {}", self.dir.display()), source: e }),
        }
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(io_err(format!("cannot write {}", tmp.display())))?;
    fs::rename(&tmp, path).map_err(io_err(format!("cannot write {}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net() -> (tempfile::TempDir, ProjectNet) {
        let root = tempfile::tempdir().unwrap();
        let net = ProjectNet::new(root.path(), "demo").unwrap();
        (root, net)
    }

    #[test]
    fn the_directory_lives_under_projects_and_the_name_is_checked() {
        let (root, net) = net();
        assert_eq!(net.dir(), root.path().join("projects/demo"));
        for bad in ["", "../x", "a/b", ".hidden"] {
            assert!(matches!(ProjectNet::new(root.path(), bad), Err(RuntimeError::InvalidName(_))), "{bad:?}");
        }
    }

    #[test]
    fn info_roundtrip_and_missing_info() {
        let (_r, net) = net();
        assert_eq!(net.info().unwrap(), None);
        let info = NetInfo { pid: 42, published: vec![PortMap { host: 8080, container: 80 }] };
        net.create_dir().unwrap();
        net.write_info(&info).unwrap();
        assert_eq!(net.info().unwrap(), Some(info));
        std::fs::write(net.info_path(), "{").unwrap();
        assert!(matches!(net.info(), Err(RuntimeError::Json { .. })));
    }

    #[test]
    fn the_holder_lock_defines_liveness_and_guards_the_pid() {
        let (_r, net) = net();
        assert!(!net.is_alive().unwrap(), "nothing there yet");
        net.write_info(&NetInfo { pid: 7, published: vec![] }).unwrap();
        assert_eq!(net.alive_pid().unwrap(), None, "a recorded pid without a holder is not trusted");

        let held = net.try_acquire().unwrap().expect("free");
        assert!(net.try_acquire().unwrap().is_none(), "exclusive");
        assert!(net.is_alive().unwrap());
        assert_eq!(net.alive_pid().unwrap(), Some(7));
        drop(held);
        assert!(!net.is_alive().unwrap());
        assert_eq!(net.alive_pid().unwrap(), None);
    }

    #[test]
    fn the_hosts_file_maps_every_name_once_to_the_shared_loopback() {
        let (_r, net) = net();
        net.write_hosts(&["web".into(), "db".into(), "web".into(), "demo-db-1".into()]).unwrap();
        let hosts = std::fs::read_to_string(net.hosts_path()).unwrap();
        assert_eq!(hosts, "127.0.0.1\tlocalhost db demo-db-1 web\n::1\tlocalhost ip6-localhost ip6-loopback\n");
        net.write_hosts(&["api".into()]).unwrap();
        assert!(std::fs::read_to_string(net.hosts_path()).unwrap().starts_with("127.0.0.1\tlocalhost api\n"));
    }

    #[test]
    fn remove_deletes_everything_and_is_idempotent() {
        let (_r, net) = net();
        net.write_hosts(&["a".into()]).unwrap();
        net.remove().unwrap();
        assert!(!net.dir().exists());
        net.remove().unwrap();
    }

    #[test]
    fn ports_sort_by_host_then_container() {
        let mut ports = vec![PortMap { host: 90, container: 1 }, PortMap { host: 80, container: 2 }, PortMap { host: 80, container: 1 }];
        ports.sort();
        assert_eq!(ports.iter().map(|p| (p.host, p.container)).collect::<Vec<_>>(), [(80, 1), (80, 2), (90, 1)]);
    }
}
