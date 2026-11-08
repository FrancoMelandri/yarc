//! On-disk container records: `<root>/containers/<id>/{config.json, rootfs/}`.

use std::collections::hash_map::RandomState;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::error::{io_err, Result, RuntimeError};
use crate::spec::ContainerSpec;

const MAX_NAME: usize = 64;

#[derive(Debug, Clone)]
pub struct ContainerStore {
    dir: PathBuf,
}

impl ContainerStore {
    pub fn open(root: &Path) -> Result<Self> {
        let dir = root.join("containers");
        fs::create_dir_all(&dir).map_err(io_err(format!("cannot create {}", dir.display())))?;
        Ok(ContainerStore { dir })
    }

    pub fn container_dir(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    pub fn rootfs_path(&self, id: &str) -> PathBuf {
        self.container_dir(id).join("rootfs")
    }

    pub fn config_path(&self, id: &str) -> PathBuf {
        self.container_dir(id).join("config.json")
    }

    /// Reserves an id (the given name, or a random 12-hex-digit one) by creating
    /// its directory. The rootfs directory itself is *not* created, so it can be
    /// unpacked into.
    pub fn allocate(&self, name: Option<&str>) -> Result<String> {
        if let Some(name) = name {
            check_id(name)?;
            return match fs::create_dir(self.container_dir(name)) {
                Ok(()) => Ok(name.to_string()),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => Err(RuntimeError::AlreadyExists(name.to_string())),
                Err(e) => Err(RuntimeError::Io { context: format!("cannot create container {name}"), source: e }),
            };
        }
        loop {
            let id = random_id();
            match fs::create_dir(self.container_dir(&id)) {
                Ok(()) => return Ok(id),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(RuntimeError::Io { context: format!("cannot create container {id}"), source: e }),
            }
        }
    }

    pub fn save(&self, spec: &ContainerSpec) -> Result<()> {
        check_id(&spec.id)?;
        let path = self.config_path(&spec.id);
        let json = serde_json::to_vec_pretty(spec).map_err(|source| RuntimeError::Json { path: path.clone(), source })?;
        // Write-then-rename so a crash never leaves a half-written config.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json).map_err(io_err(format!("cannot write {}", tmp.display())))?;
        fs::rename(&tmp, &path).map_err(io_err(format!("cannot write {}", path.display())))
    }

    pub fn load(&self, id: &str) -> Result<ContainerSpec> {
        check_id(id).map_err(|_| RuntimeError::NotFound(id.to_string()))?;
        let path = self.config_path(id);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => return Err(RuntimeError::NotFound(id.to_string())),
            Err(e) => return Err(RuntimeError::Io { context: format!("cannot read {}", path.display()), source: e }),
        };
        serde_json::from_slice(&bytes).map_err(|source| RuntimeError::Json { path, source })
    }

    /// All containers with a readable config, sorted by id. Half-created
    /// directories (no config) are ignored.
    pub fn list(&self) -> Result<Vec<ContainerSpec>> {
        let mut out = Vec::new();
        let entries = fs::read_dir(&self.dir).map_err(io_err(format!("cannot read {}", self.dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(io_err("cannot read containers directory"))?;
            let Some(id) = entry.file_name().to_str().map(str::to_string) else { continue };
            match self.load(&id) {
                Ok(spec) => out.push(spec),
                Err(RuntimeError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// Deletes the container directory, including a partially created one.
    pub fn remove(&self, id: &str) -> Result<()> {
        check_id(id).map_err(|_| RuntimeError::NotFound(id.to_string()))?;
        let dir = self.container_dir(id);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Err(RuntimeError::NotFound(id.to_string())),
            Err(e) => Err(RuntimeError::Io { context: format!("cannot remove {}", dir.display()), source: e }),
        }
    }
}

/// Ids double as directory names, so this also blocks path traversal.
fn check_id(id: &str) -> Result<()> {
    let mut chars = id.chars();
    let ok = id.len() <= MAX_NAME
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if ok { Ok(()) } else { Err(RuntimeError::InvalidName(id.to_string())) }
}

/// `RandomState` is seeded randomly per instance by the standard library, which
/// is plenty for avoiding collisions between container ids.
fn random_id() -> String {
    let n = RandomState::new().build_hasher().finish();
    format!("{:012x}", n & 0xffff_ffff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Network, Process};

    fn store() -> (tempfile::TempDir, ContainerStore) {
        let root = tempfile::tempdir().unwrap();
        let store = ContainerStore::open(root.path()).unwrap();
        (root, store)
    }

    fn spec(store: &ContainerStore, id: &str) -> ContainerSpec {
        let p = Process { args: vec!["echo".into(), "hi".into()], env: vec![("A".into(), "1".into())], working_dir: "/".into(), user: None };
        ContainerSpec::new(id, "alpine:3.20", store.rootfs_path(id), p, Network::Host)
    }

    #[test]
    fn allocate_save_load_roundtrip() {
        let (_r, store) = store();
        let id = store.allocate(None).unwrap();
        assert_eq!(id.len(), 12);
        assert!(store.container_dir(&id).is_dir());
        assert!(!store.rootfs_path(&id).exists(), "rootfs is left for the unpacker to create");
        let s = spec(&store, &id);
        store.save(&s).unwrap();
        assert_eq!(store.load(&id).unwrap(), s);
        assert!(!store.config_path(&id).with_extension("json.tmp").exists());
    }

    #[test]
    fn random_ids_differ() {
        let (_r, store) = store();
        let a = store.allocate(None).unwrap();
        let b = store.allocate(None).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn names_are_validated_and_unique() {
        let (_r, store) = store();
        assert_eq!(store.allocate(Some("db-1.test_x")).unwrap(), "db-1.test_x");
        assert!(matches!(store.allocate(Some("db-1.test_x")), Err(RuntimeError::AlreadyExists(_))));
        for bad in ["", "../x", "a/b", "-a", ".hidden", "a b", &"x".repeat(65)] {
            assert!(matches!(store.allocate(Some(bad)), Err(RuntimeError::InvalidName(_))), "{bad:?}");
        }
    }

    #[test]
    fn load_unknown_or_hostile_ids() {
        let (_r, store) = store();
        assert!(matches!(store.load("nope"), Err(RuntimeError::NotFound(_))));
        assert!(matches!(store.load("../../etc"), Err(RuntimeError::NotFound(_))));
        assert!(matches!(store.remove("../.."), Err(RuntimeError::NotFound(_))));
    }

    #[test]
    fn list_is_sorted_and_skips_unfinished_containers() {
        let (_r, store) = store();
        for name in ["b", "a"] {
            store.allocate(Some(name)).unwrap();
            store.save(&spec(&store, name)).unwrap();
        }
        store.allocate(Some("unfinished")).unwrap();
        let ids: Vec<_> = store.list().unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn remove_deletes_everything_including_rootfs() {
        let (_r, store) = store();
        let id = store.allocate(Some("c")).unwrap();
        fs::create_dir_all(store.rootfs_path(&id).join("etc")).unwrap();
        fs::write(store.rootfs_path(&id).join("etc/hosts"), "x").unwrap();
        store.save(&spec(&store, &id)).unwrap();
        store.remove(&id).unwrap();
        assert!(!store.container_dir(&id).exists());
        assert!(matches!(store.remove(&id), Err(RuntimeError::NotFound(_))));
    }

    #[test]
    fn corrupt_config_is_reported() {
        let (_r, store) = store();
        let id = store.allocate(Some("bad")).unwrap();
        fs::write(store.config_path(&id), "{").unwrap();
        assert!(matches!(store.load(&id), Err(RuntimeError::Json { .. })));
    }
}
