//! Unpack image layers into a root filesystem directory.
//!
//! Layers are untrusted input, so entries are never extracted with a plain
//! `join`. Every existing parent component is resolved the way a chroot would:
//! absolute symlink targets restart at the rootfs, `..` is clamped at the
//! rootfs, and the number of symlinks followed is bounded. A symlink created by
//! one layer can therefore never redirect a later write to the host.
//!
//! Not handled (yet): device nodes / fifos (skipped and counted), xattrs,
//! mtimes, sparse files, zstd layers.

use std::collections::{HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Permissions};
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{lchown, symlink, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use tar::EntryType;

use crate::digest::Digest;
use crate::error::{OciError, Result};
use crate::reference::Reference;
use crate::store::{io_err, BlobStore};

const MAX_SYMLINKS: usize = 40;
const WHITEOUT_PREFIX: &[u8] = b".wh.";
const OPAQUE_MARKER: &[u8] = b".wh..wh..opq";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UnpackReport {
    pub files: usize,
    pub dirs: usize,
    pub symlinks: usize,
    pub hardlinks: usize,
    pub whiteouts: usize,
    /// Device nodes and fifos, which are not created.
    pub skipped_special: usize,
}

impl UnpackReport {
    fn add(&mut self, o: &UnpackReport) {
        self.files += o.files;
        self.dirs += o.dirs;
        self.symlinks += o.symlinks;
        self.hardlinks += o.hardlinks;
        self.whiteouts += o.whiteouts;
        self.skipped_special += o.skipped_special;
    }
}

/// Unpack every layer of a pulled image, in order, into `dest`.
///
/// `dest` must not exist or be an empty directory.
pub fn unpack_image(store: &BlobStore, reference: &Reference, dest: &Path) -> Result<UnpackReport> {
    let image = store.load_image(reference)?;
    prepare_dest(dest)?;
    let mut total = UnpackReport::default();
    for layer in &image.manifest.layers {
        let blob = store.open_blob(&Digest::parse(&layer.digest)?)?;
        total.add(&unpack_layer(blob, dest)?);
    }
    Ok(total)
}

fn prepare_dest(dest: &Path) -> Result<()> {
    match fs::read_dir(dest) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(OciError::DestNotEmpty(dest.display().to_string()));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(dest).map_err(io_err(dest))?;
        }
        Err(_) => return Err(OciError::DestNotEmpty(dest.display().to_string())),
    }
    Ok(())
}

/// Apply one layer (plain or gzip-compressed tar) on top of what is already in `dest`.
pub fn unpack_layer<R: Read>(reader: R, dest: &Path) -> Result<UnpackReport> {
    fs::create_dir_all(dest).map_err(io_err(dest))?;
    let reader = open_layer(reader)?;
    let mut unpacker = Unpacker::new(dest);
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().map_err(tar_err)? {
        let mut entry = entry.map_err(tar_err)?;
        unpacker.apply(&mut entry)?;
    }
    unpacker.finish()
}

fn tar_err(e: io::Error) -> OciError {
    OciError::BadLayer(e.to_string())
}

/// Sniff the compression from the magic bytes (media types are not reliable).
fn open_layer<'r, R: Read + 'r>(reader: R) -> Result<Box<dyn Read + 'r>> {
    let mut reader = BufReader::new(reader);
    let magic = reader.fill_buf().map_err(tar_err)?;
    if magic.starts_with(&[0x1f, 0x8b]) {
        Ok(Box::new(flate2::read::MultiGzDecoder::new(reader)))
    } else if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Err(OciError::Unsupported("zstd-compressed layers".into()))
    } else {
        Ok(Box::new(reader))
    }
}

struct DirFixup {
    path: PathBuf,
    mode: u32,
    uid: u32,
    gid: u32,
}

struct Unpacker<'a> {
    root: &'a Path,
    is_root: bool,
    /// Everything this layer wrote, so an opaque-dir marker spares it.
    created: HashSet<PathBuf>,
    dirs: Vec<DirFixup>,
    report: UnpackReport,
}

impl<'a> Unpacker<'a> {
    fn new(root: &'a Path) -> Self {
        Unpacker {
            root,
            // SAFETY: geteuid has no preconditions and cannot fail.
            is_root: unsafe { libc::geteuid() } == 0,
            created: HashSet::new(),
            dirs: Vec::new(),
            report: UnpackReport::default(),
        }
    }

    fn apply<R: Read>(&mut self, entry: &mut tar::Entry<'_, R>) -> Result<()> {
        let path = entry.path().map_err(tar_err)?.into_owned();
        let kind = entry.header().entry_type();
        let comps = normalize(&path)?;
        let Some((name, parent_comps)) = comps.split_last() else {
            return Ok(()); // the root directory entry itself
        };

        // Whiteouts first: they never create anything.
        if name.as_bytes() == OPAQUE_MARKER {
            let dir = self.root.join(self.resolve(parent_comps, true)?);
            self.make_opaque(&dir)?;
            self.report.whiteouts += 1;
            return Ok(());
        }
        if let Some(victim) = name.as_bytes().strip_prefix(WHITEOUT_PREFIX) {
            if !victim.is_empty() {
                let dir = self.root.join(self.resolve(parent_comps, true)?);
                remove_any(&dir.join(OsStr::from_bytes(victim)))?;
                self.report.whiteouts += 1;
            }
            return Ok(());
        }

        match kind {
            EntryType::Directory
            | EntryType::Regular
            | EntryType::Continuous
            | EntryType::Symlink
            | EntryType::Link => {}
            EntryType::Char | EntryType::Block | EntryType::Fifo => {
                self.report.skipped_special += 1;
                return Ok(());
            }
            EntryType::GNUSparse => {
                return Err(OciError::Unsupported("sparse files in layers".into()))
            }
            _ => return Ok(()), // pax / gnu extension headers etc.
        }

        let parent = self.root.join(self.resolve(parent_comps, true)?);
        fs::create_dir_all(&parent).map_err(io_err(&parent))?;
        let dest = parent.join(name);
        let mode = entry.header().mode().map_err(tar_err)? & 0o7777;
        let uid = entry.header().uid().map_err(tar_err)? as u32;
        let gid = entry.header().gid().map_err(tar_err)? as u32;

        match kind {
            EntryType::Directory => {
                match fs::symlink_metadata(&dest) {
                    Ok(md) if md.is_dir() => {}
                    Ok(_) => {
                        fs::remove_file(&dest).map_err(io_err(&dest))?;
                        fs::create_dir(&dest).map_err(io_err(&dest))?;
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        fs::create_dir(&dest).map_err(io_err(&dest))?;
                    }
                    Err(e) => return Err(io_err(&dest)(e)),
                }
                self.dirs.push(DirFixup { path: dest.clone(), mode, uid, gid });
                self.report.dirs += 1;
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .map_err(tar_err)?
                    .ok_or_else(|| OciError::BadLayer(format!("symlink '{}' has no target", path.display())))?
                    .into_owned();
                remove_any(&dest)?;
                symlink(&target, &dest).map_err(io_err(&dest))?;
                self.chown(&dest, uid, gid)?;
                self.report.symlinks += 1;
            }
            EntryType::Link => {
                let target = entry
                    .link_name()
                    .map_err(tar_err)?
                    .ok_or_else(|| OciError::BadLayer(format!("hardlink '{}' has no target", path.display())))?
                    .into_owned();
                let target_comps = normalize(&target)?;
                let Some((target_name, target_parents)) = target_comps.split_last() else {
                    return Err(OciError::BadLayer(format!("hardlink '{}' points at the root", path.display())));
                };
                let source = self.root.join(self.resolve(target_parents, true)?).join(target_name);
                remove_any(&dest)?;
                fs::hard_link(&source, &dest).map_err(io_err(&source))?;
                self.report.hardlinks += 1;
            }
            _ => {
                // Regular / Continuous
                remove_any(&dest)?;
                let mut file = File::create_new(&dest).map_err(io_err(&dest))?;
                io::copy(entry, &mut file).map_err(io_err(&dest))?;
                drop(file);
                // chown before chmod: chown clears setuid/setgid bits.
                self.chown(&dest, uid, gid)?;
                fs::set_permissions(&dest, Permissions::from_mode(mode)).map_err(io_err(&dest))?;
                self.report.files += 1;
            }
        }

        self.mark_created(&dest);
        Ok(())
    }

    /// Resolve `comps` inside the rootfs, following symlinks chroot-style.
    /// Returns a path relative to the root. The last component is only followed
    /// when `follow_last` is set. Components that do not exist yet are kept as is.
    fn resolve(&self, comps: &[OsString], follow_last: bool) -> Result<PathBuf> {
        let mut resolved: Vec<OsString> = Vec::new();
        let mut pending: VecDeque<OsString> = comps.iter().cloned().collect();
        let mut links = 0;
        while let Some(c) = pending.pop_front() {
            match c.to_str() {
                Some(".") => continue,
                Some("..") => {
                    resolved.pop(); // clamped at the rootfs
                    continue;
                }
                _ => {}
            }
            let is_last = pending.is_empty();
            let mut candidate = self.root.to_path_buf();
            candidate.extend(&resolved);
            candidate.push(&c);
            match fs::symlink_metadata(&candidate) {
                Ok(md) if md.file_type().is_symlink() && (!is_last || follow_last) => {
                    links += 1;
                    if links > MAX_SYMLINKS {
                        return Err(OciError::TooManySymlinks(candidate.display().to_string()));
                    }
                    let target = fs::read_link(&candidate).map_err(io_err(&candidate))?;
                    if target.is_absolute() {
                        resolved.clear();
                    }
                    let injected: Vec<OsString> = target
                        .components()
                        .filter_map(|tc| match tc {
                            Component::Normal(n) => Some(n.to_os_string()),
                            Component::ParentDir => Some(OsString::from("..")),
                            _ => None,
                        })
                        .collect();
                    for tc in injected.into_iter().rev() {
                        pending.push_front(tc);
                    }
                }
                _ => resolved.push(c),
            }
        }
        Ok(resolved.iter().collect())
    }

    /// Opaque whiteout: drop everything lower layers put in `dir`, keep what this layer wrote.
    fn make_opaque(&self, dir: &Path) -> Result<()> {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_err(dir)(e)),
        };
        for entry in entries {
            let child = entry.map_err(io_err(dir))?.path();
            if !self.created.contains(&child) {
                remove_any(&child)?;
            }
        }
        Ok(())
    }

    /// Remember `dest` and its ancestors (up to the root) as written by this layer.
    fn mark_created(&mut self, dest: &Path) {
        self.created.insert(dest.to_path_buf());
        let mut ancestor = dest.parent();
        while let Some(a) = ancestor {
            if a == self.root || !self.created.insert(a.to_path_buf()) {
                break;
            }
            ancestor = a.parent();
        }
    }

    fn chown(&self, path: &Path, uid: u32, gid: u32) -> Result<()> {
        if !self.is_root {
            return Ok(());
        }
        lchown(path, Some(uid), Some(gid)).map_err(io_err(path))
    }

    /// Apply directory ownership/permissions last (deepest first), so read-only
    /// directories do not block the extraction of their own contents.
    fn finish(mut self) -> Result<UnpackReport> {
        self.dirs.sort_by_key(|d| std::cmp::Reverse(d.path.components().count()));
        for d in std::mem::take(&mut self.dirs) {
            self.chown(&d.path, d.uid, d.gid)?;
            // Unprivileged runs keep owner rwx so later layers can still write.
            let mode = if self.is_root { d.mode } else { d.mode | 0o700 };
            fs::set_permissions(&d.path, Permissions::from_mode(mode)).map_err(io_err(&d.path))?;
        }
        Ok(self.report)
    }
}

/// Split a tar path into plain components. Leading `/` and `.` are tolerated;
/// `..` is rejected.
fn normalize(path: &Path) -> Result<Vec<OsString>> {
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(n) => out.push(n.to_os_string()),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => return Err(OciError::UnsafePath(path.display().to_string())),
        }
    }
    Ok(out)
}

/// Remove a file, symlink or directory tree without following symlinks. Missing is fine.
fn remove_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(md) if md.is_dir() => fs::remove_dir_all(path).map_err(io_err(path)),
        Ok(_) => fs::remove_file(path).map_err(io_err(path)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(path)(e)),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;

    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tar::Header;

    use super::*;

    // ---- tar builders (raw headers, so we can write paths `tar::Builder` refuses) ----

    fn raw(path: &str, kind: EntryType, link: &str, data: &[u8], mode: u32) -> Vec<u8> {
        let mut h = Header::new_gnu();
        h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
        h.as_old_mut().linkname[..link.len()].copy_from_slice(link.as_bytes());
        h.set_entry_type(kind);
        h.set_mode(mode);
        h.set_uid(0);
        h.set_gid(0);
        h.set_size(data.len() as u64);
        h.set_cksum();
        let mut out = h.as_bytes().to_vec();
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
        out
    }

    fn file(path: &str, data: &str) -> Vec<u8> {
        raw(path, EntryType::Regular, "", data.as_bytes(), 0o644)
    }
    fn exe(path: &str) -> Vec<u8> {
        raw(path, EntryType::Regular, "", b"#!/bin/sh\n", 0o755)
    }
    fn dir(path: &str) -> Vec<u8> {
        raw(path, EntryType::Directory, "", b"", 0o755)
    }
    fn sym(path: &str, target: &str) -> Vec<u8> {
        raw(path, EntryType::Symlink, target, b"", 0o777)
    }
    fn hard(path: &str, target: &str) -> Vec<u8> {
        raw(path, EntryType::Link, target, b"", 0o644)
    }
    fn tarball(entries: Vec<Vec<u8>>) -> Vec<u8> {
        let mut out: Vec<u8> = entries.concat();
        out.extend_from_slice(&[0u8; 1024]);
        out
    }
    fn gz(data: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// `base/rootfs` is the unpack target; `base` lets tests detect writes outside it.
    struct Env {
        base: tempfile::TempDir,
        root: PathBuf,
    }

    fn env() -> Env {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("rootfs");
        Env { base, root }
    }

    impl Env {
        fn layer(&self, entries: Vec<Vec<u8>>) -> Result<UnpackReport> {
            unpack_layer(&tarball(entries)[..], &self.root)
        }
        fn read(&self, rel: &str) -> String {
            fs::read_to_string(self.root.join(rel)).unwrap()
        }
        fn exists(&self, rel: &str) -> bool {
            fs::symlink_metadata(self.root.join(rel)).is_ok()
        }
        fn names(&self, rel: &str) -> Vec<String> {
            let mut v: Vec<String> = fs::read_dir(self.root.join(rel))
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        }
        fn nothing_outside_root(&self) {
            let outside: Vec<_> = fs::read_dir(self.base.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(outside, vec!["rootfs"], "something was written outside the rootfs");
        }
    }

    // ---- basics ----

    #[test]
    fn extracts_files_dirs_modes_and_implicit_parents() {
        let e = env();
        let report = e
            .layer(vec![dir("etc/"), file("etc/hostname", "yarc\n"), exe("usr/bin/run"), file("./a/b/c.txt", "deep")])
            .unwrap();
        assert_eq!(e.read("etc/hostname"), "yarc\n");
        assert_eq!(e.read("a/b/c.txt"), "deep"); // parents created implicitly
        let mode = fs::metadata(e.root.join("usr/bin/run")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(report.files, 3);
        assert_eq!(report.dirs, 1);
    }

    #[test]
    fn absolute_entry_paths_land_inside_the_root() {
        let e = env();
        e.layer(vec![file("/etc/passwd", "root")]).unwrap();
        assert_eq!(e.read("etc/passwd"), "root");
        e.nothing_outside_root();
    }

    #[test]
    fn rejects_dotdot_entries() {
        let e = env();
        let err = e.layer(vec![file("../escape.txt", "x")]).unwrap_err();
        assert!(matches!(err, OciError::UnsafePath(_)), "{err}");
        let err = e.layer(vec![file("a/../../escape.txt", "x")]).unwrap_err();
        assert!(matches!(err, OciError::UnsafePath(_)), "{err}");
        e.nothing_outside_root();
    }

    #[test]
    fn read_only_directory_does_not_block_its_contents() {
        let e = env();
        e.layer(vec![raw("locked/", EntryType::Directory, "", b"", 0o500), file("locked/f", "ok")]).unwrap();
        assert_eq!(e.read("locked/f"), "ok");
    }

    #[test]
    fn special_files_are_skipped_and_counted() {
        let e = env();
        let report = e
            .layer(vec![raw("dev/null", EntryType::Char, "", b"", 0o666), raw("dev/fifo", EntryType::Fifo, "", b"", 0o644)])
            .unwrap();
        assert_eq!(report.skipped_special, 2);
        assert!(!e.exists("dev/null"));
    }

    // ---- layering / whiteouts ----

    #[test]
    fn upper_layer_overrides_and_whiteouts_delete() {
        let e = env();
        e.layer(vec![file("a.txt", "old"), file("b.txt", "keep"), dir("d/"), file("d/x", "x"), file("gone", "g")]).unwrap();
        let report = e
            .layer(vec![file("a.txt", "new"), file(".wh.gone", ""), file(".wh.d", ""), file(".wh.never-existed", "")])
            .unwrap();
        assert_eq!(e.read("a.txt"), "new");
        assert_eq!(e.read("b.txt"), "keep");
        assert!(!e.exists("gone"));
        assert!(!e.exists("d"), "whiteout removes whole directories");
        assert!(!e.exists(".wh.gone"), "whiteout markers are never extracted");
        assert_eq!(report.whiteouts, 3);
    }

    #[test]
    fn opaque_whiteout_hides_lower_content_but_keeps_same_layer_content() {
        for marker_first in [true, false] {
            let e = env();
            e.layer(vec![dir("dir/"), file("dir/a", "a"), file("dir/b", "b"), file("keep", "k")]).unwrap();
            let upper = if marker_first {
                vec![file("dir/.wh..wh..opq", ""), file("dir/c", "c")]
            } else {
                vec![file("dir/c", "c"), file("dir/.wh..wh..opq", "")]
            };
            e.layer(upper).unwrap();
            assert_eq!(e.names("dir"), vec!["c"], "marker_first={marker_first}");
            assert_eq!(e.read("keep"), "k");
        }
    }

    #[test]
    fn type_changes_between_layers() {
        let e = env();
        e.layer(vec![file("a", "was file"), dir("d/"), file("d/f", "x"), dir("t/"), sym("s", "t")]).unwrap();
        e.layer(vec![dir("a/"), file("d", "now file"), dir("s/"), file("s/g", "g")]).unwrap();
        assert!(e.root.join("a").is_dir());
        assert_eq!(e.read("d"), "now file");
        assert!(fs::symlink_metadata(e.root.join("s")).unwrap().is_dir(), "symlink replaced by a real dir");
        assert_eq!(e.read("s/g"), "g");
        assert!(e.names("t").is_empty());
    }

    // ---- links ----

    #[test]
    fn hardlinks_share_an_inode_across_directories_and_layers() {
        let e = env();
        e.layer(vec![file("a", "data")]).unwrap();
        e.layer(vec![hard("sub/b", "a")]).unwrap();
        assert_eq!(e.read("sub/b"), "data");
        let (ia, ib) = (fs::metadata(e.root.join("a")).unwrap().ino(), fs::metadata(e.root.join("sub/b")).unwrap().ino());
        assert_eq!(ia, ib);
    }

    #[test]
    fn symlinks_keep_their_target_text() {
        let e = env();
        e.layer(vec![sym("bin/sh", "busybox"), sym("abs", "/usr/lib")]).unwrap();
        assert_eq!(fs::read_link(e.root.join("bin/sh")).unwrap(), PathBuf::from("busybox"));
        assert_eq!(fs::read_link(e.root.join("abs")).unwrap(), PathBuf::from("/usr/lib"));
    }

    #[test]
    fn absolute_symlink_dirs_resolve_inside_the_rootfs() {
        let e = env();
        e.layer(vec![dir("usr/"), dir("usr/lib/"), sym("lib", "/usr/lib")]).unwrap();
        e.layer(vec![file("lib/libc.so", "so")]).unwrap();
        assert_eq!(e.read("usr/lib/libc.so"), "so");
        e.nothing_outside_root();
    }

    #[test]
    fn symlink_escapes_are_confined_to_the_rootfs() {
        let e = env();
        e.layer(vec![sym("up", "../../../../outside"), sym("abs", "/etc")]).unwrap();
        e.layer(vec![file("up/pwned", "x"), file("abs/pwned2", "y")]).unwrap();
        assert_eq!(e.read("outside/pwned"), "x");
        assert_eq!(e.read("etc/pwned2"), "y");
        e.nothing_outside_root();
    }

    #[test]
    fn symlink_escape_via_hardlink_target_and_whiteout_is_confined() {
        let e = env();
        e.layer(vec![sym("up", "../.."), dir("outside_dir/")]).unwrap();
        // whiteout through the symlink must delete inside the rootfs only
        e.layer(vec![file("up/.wh.outside_dir", "")]).unwrap();
        assert!(!e.exists("outside_dir"));
        e.nothing_outside_root();
        // hardlink source resolved through the symlink stays inside too
        let err = e.layer(vec![hard("h", "up/nonexistent-file")]).unwrap_err();
        assert!(matches!(err, OciError::Io { .. }), "{err}");
        e.nothing_outside_root();
    }

    #[test]
    fn symlink_loops_are_detected() {
        let e = env();
        e.layer(vec![sym("a", "b"), sym("b", "a")]).unwrap();
        let err = e.layer(vec![file("a/x", "x")]).unwrap_err();
        assert!(matches!(err, OciError::TooManySymlinks(_)), "{err}");
    }

    // ---- compression ----

    #[test]
    fn gzip_layers_are_detected_by_magic() {
        let e = env();
        let layer = gz(&tarball(vec![file("z.txt", "zipped")]));
        unpack_layer(&layer[..], &e.root).unwrap();
        assert_eq!(e.read("z.txt"), "zipped");
    }

    #[test]
    fn zstd_and_garbage_are_rejected_clearly() {
        let e = env();
        let err = unpack_layer(&[0x28u8, 0xb5, 0x2f, 0xfd, 0, 0][..], &e.root).unwrap_err();
        assert!(matches!(err, OciError::Unsupported(_)), "{err}");
        let err = unpack_layer(&b"this is definitely not a tar archive"[..], &e.root).unwrap_err();
        assert!(matches!(err, OciError::BadLayer(_)), "{err}");
    }

    // ---- whole image from the store ----

    fn store_with_image(layers: &[Vec<u8>]) -> (tempfile::TempDir, BlobStore, Reference) {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path()).unwrap();
        let config = br#"{"architecture":"amd64","os":"linux","config":{"Cmd":["/bin/sh"]}}"#;
        let config_digest = Digest::of(config);
        store.put_bytes(&config_digest, config).unwrap();
        let mut descs = Vec::new();
        for l in layers {
            let d = Digest::of(l);
            store.put_bytes(&d, l).unwrap();
            descs.push(format!(r#"{{"mediaType":"l","digest":"{d}","size":{}}}"#, l.len()));
        }
        let manifest = format!(
            r#"{{"schemaVersion":2,"config":{{"mediaType":"c","digest":"{config_digest}","size":{}}},"layers":[{}]}}"#,
            config.len(),
            descs.join(",")
        );
        let md = Digest::of(manifest.as_bytes());
        store.put_bytes(&md, manifest.as_bytes()).unwrap();
        let reference: Reference = "test/img:1".parse().unwrap();
        store.set_ref(&reference, &md).unwrap();
        (dir, store, reference)
    }

    #[test]
    fn unpacks_a_whole_image_from_the_store() {
        let layers = vec![
            gz(&tarball(vec![file("a", "1"), file("b", "1"), dir("d/"), file("d/x", "x")])),
            tarball(vec![file("a", "2"), file(".wh.b", ""), file(".wh.d", "")]),
        ];
        let (_s, store, reference) = store_with_image(&layers);
        let e = env();

        let report = unpack_image(&store, &reference, &e.root).unwrap();

        assert_eq!(e.names("."), vec!["a"]);
        assert_eq!(e.read("a"), "2");
        assert_eq!(report.whiteouts, 2);
        assert_eq!(store.load_image(&reference).unwrap().config.config.cmd, Some(vec!["/bin/sh".to_string()]));
    }

    #[test]
    fn unknown_image_and_non_empty_destination_are_errors() {
        let (_s, store, reference) = store_with_image(&[tarball(vec![file("a", "1")])]);
        let e = env();

        let missing: Reference = "never/pulled".parse().unwrap();
        assert!(matches!(unpack_image(&store, &missing, &e.root), Err(OciError::NotFound(_))));

        fs::create_dir_all(&e.root).unwrap();
        fs::write(e.root.join("existing"), "x").unwrap();
        assert!(matches!(unpack_image(&store, &reference, &e.root), Err(OciError::DestNotEmpty(_))));
    }
}
