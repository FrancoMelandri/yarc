//! Content-addressed blob store.
//!
//! Layout under `<root>`:
//! ```text
//! blobs/sha256/<hex>   verified blobs (manifests, configs, layers)
//! tmp/                 in-flight downloads (never read by anyone)
//! refs/<hex>.json      image reference -> manifest digest
//! ```
//! A blob only appears in `blobs/` after its size and digest were verified, via an
//! atomic rename, so readers never see partial or corrupt data.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest as _, Sha256};

use crate::digest::Digest;
use crate::error::{OciError, Result};
use crate::manifest::{parse_config, parse_manifest, ImageConfig, ImageManifest, Manifest};
use crate::reference::Reference;

/// A fully pulled image, read back from the store.
#[derive(Debug, Clone)]
pub struct StoredImage {
    pub reference: Reference,
    pub manifest_digest: Digest,
    pub manifest: ImageManifest,
    pub config: ImageConfig,
}

/// What [`BlobStore::remove_image`] deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveReport {
    pub digest: Digest,
    pub blobs_removed: usize,
    pub bytes_freed: u64,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const BUF_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

/// Removes the temp file when dropped (success path has already renamed it away).
struct TmpGuard(PathBuf);

impl Drop for TmpGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(crate) fn io_err(path: &Path) -> impl Fn(io::Error) -> OciError + '_ {
    move |e| OciError::Io { path: path.display().to_string(), msg: e.to_string() }
}

impl BlobStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let store = BlobStore { root: root.into() };
        for dir in [store.blobs_dir(), store.tmp_dir(), store.refs_dir()] {
            fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        }
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs").join("sha256")
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    fn refs_dir(&self) -> PathBuf {
        self.root.join("refs")
    }

    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.blobs_dir().join(digest.hex())
    }

    pub fn contains(&self, digest: &Digest) -> bool {
        self.blob_path(digest).is_file()
    }

    pub fn open_blob(&self, digest: &Digest) -> Result<File> {
        let p = self.blob_path(digest);
        File::open(&p).map_err(io_err(&p))
    }

    pub fn read_blob(&self, digest: &Digest) -> Result<Vec<u8>> {
        let p = self.blob_path(digest);
        fs::read(&p).map_err(io_err(&p))
    }

    pub fn put_bytes(&self, digest: &Digest, bytes: &[u8]) -> Result<()> {
        self.put_stream(digest, bytes, Some(bytes.len() as u64)).map(|_| ())
    }

    /// Stream `reader` into the store, hashing as it goes.
    ///
    /// Fails (leaving nothing behind) if the data is larger than `expected_size`, or
    /// its size or digest do not match. Returns the number of bytes written, or 0 if
    /// the blob was already present (the reader is then not consumed).
    pub fn put_stream(&self, digest: &Digest, reader: impl Read, expected_size: Option<u64>) -> Result<u64> {
        let dest = self.blob_path(digest);
        if dest.is_file() {
            return Ok(0);
        }

        let tmp = self.tmp_path(digest);
        let _guard = TmpGuard(tmp.clone());
        let mut file = File::create_new(&tmp).map_err(io_err(&tmp))?;

        // Read at most one byte past the expected size: enough to detect an oversized
        // stream without trusting the server to ever stop sending.
        let limit = expected_size.map_or(u64::MAX, |n| n.saturating_add(1));
        let mut reader = reader.take(limit);
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; BUF_SIZE];
        let mut total = 0u64;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(OciError::Transport(e.to_string())),
            };
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n]).map_err(io_err(&tmp))?;
            total += n as u64;
        }

        if let Some(expected) = expected_size {
            if total != expected {
                return Err(OciError::SizeMismatch { what: digest.to_string(), expected, actual: total });
            }
        }
        let actual = Digest::from_hasher(hasher);
        if &actual != digest {
            return Err(OciError::DigestMismatch {
                what: "blob".into(),
                expected: digest.to_string(),
                actual: actual.to_string(),
            });
        }

        file.sync_all().map_err(io_err(&tmp))?;
        drop(file);
        match fs::rename(&tmp, &dest) {
            Ok(()) => Ok(total),
            // Another writer published the same (verified) content first.
            Err(_) if dest.is_file() => Ok(total),
            Err(e) => Err(io_err(&dest)(e)),
        }
    }

    fn tmp_path(&self, digest: &Digest) -> PathBuf {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        self.tmp_dir().join(format!("{}.{}.{}.partial", digest.hex(), std::process::id(), n))
    }

    // ---- references ---------------------------------------------------------

    fn ref_path(&self, reference: &Reference) -> PathBuf {
        let key = Digest::of(reference.to_string().as_bytes());
        self.refs_dir().join(format!("{}.json", key.hex()))
    }

    /// Record that `reference` currently resolves to the manifest `digest`.
    pub fn set_ref(&self, reference: &Reference, digest: &Digest) -> Result<()> {
        let body = serde_json::json!({
            "reference": reference.to_string(),
            "digest": digest.to_string(),
        });
        let dest = self.ref_path(reference);
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = self.tmp_dir().join(format!("ref.{}.{}.partial", std::process::id(), n));
        let _guard = TmpGuard(tmp.clone());
        fs::write(&tmp, body.to_string()).map_err(io_err(&tmp))?;
        fs::rename(&tmp, &dest).map_err(io_err(&dest))
    }

    pub fn get_ref(&self, reference: &Reference) -> Result<Option<Digest>> {
        let p = self.ref_path(reference);
        let text = match fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(&p)(e)),
        };
        let json_err = |msg: String| OciError::Json { what: "ref file".into(), msg };
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| json_err(e.to_string()))?;
        let d = v.get("digest").and_then(|d| d.as_str()).ok_or_else(|| json_err("missing 'digest'".into()))?;
        Digest::parse(d).map(Some)
    }

    /// Load a pulled image (manifest + config) by reference.
    pub fn load_image(&self, reference: &Reference) -> Result<StoredImage> {
        let manifest_digest = self.get_ref(reference)?.ok_or_else(|| OciError::NotFound(reference.to_string()))?;
        let manifest = match parse_manifest(&self.read_blob(&manifest_digest)?)? {
            Manifest::Image(m) => m,
            Manifest::Index(_) => return Err(OciError::Unsupported("stored manifest is an image index".into())),
        };
        let config = parse_config(&self.read_blob(&Digest::parse(&manifest.config.digest)?)?)?;
        Ok(StoredImage { reference: reference.clone(), manifest_digest, manifest, config })
    }

    /// Every reference in the store with the manifest it points to, sorted by name.
    pub fn list_refs(&self) -> Result<Vec<(Reference, Digest)>> {
        let dir = self.refs_dir();
        let mut out: Vec<(Reference, Digest)> = Vec::new();
        for entry in fs::read_dir(&dir).map_err(io_err(&dir))? {
            let path = entry.map_err(io_err(&dir))?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let json_err = |msg: String| OciError::Json { what: format!("ref file {}", path.display()), msg };
            let text = fs::read_to_string(&path).map_err(io_err(&path))?;
            let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| json_err(e.to_string()))?;
            let field = |k: &str| v.get(k).and_then(|x| x.as_str()).ok_or_else(|| json_err(format!("missing '{k}'")));
            let reference: Reference = field("reference")?.parse()?;
            out.push((reference, Digest::parse(field("digest")?)?));
        }
        out.sort_by_key(|(r, _)| r.to_string());
        Ok(out)
    }

    /// The manifest, config and layer blobs of the image whose manifest is `manifest_digest`.
    pub fn image_blobs(&self, manifest_digest: &Digest) -> Result<Vec<Digest>> {
        let manifest = match parse_manifest(&self.read_blob(manifest_digest)?)? {
            Manifest::Image(m) => m,
            Manifest::Index(_) => return Err(OciError::Unsupported("stored manifest is an image index".into())),
        };
        let mut blobs = vec![manifest_digest.clone(), Digest::parse(&manifest.config.digest)?];
        for layer in &manifest.layers {
            blobs.push(Digest::parse(&layer.digest)?);
        }
        Ok(blobs)
    }

    /// Forget `reference` and delete the blobs no other image uses.
    ///
    /// Containers already created from the image are unaffected (their root
    /// filesystem was unpacked). Returns `None` if the reference is unknown.
    pub fn remove_image(&self, reference: &Reference) -> Result<Option<RemoveReport>> {
        let Some(digest) = self.get_ref(reference)? else { return Ok(None) };
        let doomed = self.image_blobs(&digest)?;

        // Work out what the other images still need *before* deleting anything,
        // so an unreadable image aborts the removal instead of losing its blobs.
        let mut keep = std::collections::HashSet::new();
        for (other, other_digest) in self.list_refs()? {
            if &other != reference {
                keep.extend(self.image_blobs(&other_digest)?);
            }
        }

        let path = self.ref_path(reference);
        fs::remove_file(&path).map_err(io_err(&path))?;

        let mut report = RemoveReport { digest, blobs_removed: 0, bytes_freed: 0 };
        for blob in doomed.iter().filter(|b| !keep.contains(*b)) {
            let p = self.blob_path(blob);
            let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            match fs::remove_file(&p) {
                Ok(()) => {
                    report.blobs_removed += 1;
                    report.bytes_freed += size;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(&p)(e)),
            }
        }
        Ok(Some(report))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = BlobStore::open(dir.path()).unwrap();
        (dir, s)
    }

    fn tmp_is_empty(s: &BlobStore) -> bool {
        fs::read_dir(s.tmp_dir()).unwrap().next().is_none()
    }

    #[test]
    fn stores_and_reads_back() {
        let (_d, s) = store();
        let data = b"hello layer".repeat(10_000); // > one buffer
        let digest = Digest::of(&data);
        assert!(!s.contains(&digest));

        let n = s.put_stream(&digest, Cursor::new(&data), Some(data.len() as u64)).unwrap();
        assert_eq!(n, data.len() as u64);
        assert!(s.contains(&digest));
        assert_eq!(s.read_blob(&digest).unwrap(), data);
        assert!(tmp_is_empty(&s));
    }

    #[test]
    fn second_put_is_a_noop() {
        let (_d, s) = store();
        let digest = Digest::of(b"x");
        s.put_bytes(&digest, b"x").unwrap();
        assert_eq!(s.put_stream(&digest, Cursor::new(b"x"), Some(1)).unwrap(), 0);
    }

    #[test]
    fn digest_mismatch_leaves_nothing_behind() {
        let (_d, s) = store();
        let digest = Digest::of(b"expected");
        let err = s.put_stream(&digest, Cursor::new(b"tampered"), Some(8)).unwrap_err();
        assert!(matches!(err, OciError::DigestMismatch { .. }), "{err}");
        assert!(!s.contains(&digest));
        assert!(tmp_is_empty(&s));
    }

    #[test]
    fn size_mismatch_short_and_oversized() {
        let (_d, s) = store();
        let digest = Digest::of(b"0123456789");
        for data in [&b"01234"[..], &b"0123456789-and-more"[..]] {
            let err = s.put_stream(&digest, Cursor::new(data), Some(10)).unwrap_err();
            assert!(matches!(err, OciError::SizeMismatch { expected: 10, .. }), "{err}");
        }
        assert!(!s.contains(&digest));
        assert!(tmp_is_empty(&s));
    }

    #[test]
    fn oversized_stream_is_cut_off() {
        // A reader that never ends must not hang or fill the disk.
        let (_d, s) = store();
        let digest = Digest::of(b"whatever");
        let endless = io::repeat(b'a');
        let err = s.put_stream(&digest, endless, Some(100)).unwrap_err();
        assert!(matches!(err, OciError::SizeMismatch { actual: 101, .. }), "{err}");
        assert!(tmp_is_empty(&s));
    }

    #[test]
    fn concurrent_writers_of_same_blob() {
        let (_d, s) = store();
        let data = b"shared".repeat(50_000);
        let digest = Digest::of(&data);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    s.put_stream(&digest, Cursor::new(&data), Some(data.len() as u64)).unwrap();
                });
            }
        });
        assert_eq!(s.read_blob(&digest).unwrap(), data);
        assert!(tmp_is_empty(&s));
    }

    #[test]
    fn refs_roundtrip_and_overwrite() {
        let (_d, s) = store();
        let r: Reference = "alpine:3.20".parse().unwrap();
        assert_eq!(s.get_ref(&r).unwrap(), None);

        let d1 = Digest::of(b"one");
        let d2 = Digest::of(b"two");
        s.set_ref(&r, &d1).unwrap();
        assert_eq!(s.get_ref(&r).unwrap(), Some(d1));
        s.set_ref(&r, &d2).unwrap();
        assert_eq!(s.get_ref(&r).unwrap(), Some(d2));

        let other: Reference = "alpine:3.19".parse().unwrap();
        assert_eq!(s.get_ref(&other).unwrap(), None);
        assert!(tmp_is_empty(&s));
    }

    /// Store an image made of `layers` under `name`; returns its manifest digest.
    fn put_image(s: &BlobStore, name: &str, layers: &[&[u8]]) -> Digest {
        let config = format!(
            r#"{{"architecture":"amd64","os":"linux","created":"2026-01-02T03:04:05Z","config":{{}},"tag":"{name}"}}"#
        );
        let cd = Digest::of(config.as_bytes());
        s.put_bytes(&cd, config.as_bytes()).unwrap();
        let mut descs = Vec::new();
        for l in layers {
            let d = Digest::of(l);
            s.put_bytes(&d, l).unwrap();
            descs.push(format!(r#"{{"mediaType":"l","digest":"{d}","size":{}}}"#, l.len()));
        }
        let manifest = format!(
            r#"{{"schemaVersion":2,"config":{{"mediaType":"c","digest":"{cd}","size":{}}},"layers":[{}]}}"#,
            config.len(),
            descs.join(",")
        );
        let md = Digest::of(manifest.as_bytes());
        s.put_bytes(&md, manifest.as_bytes()).unwrap();
        s.set_ref(&name.parse().unwrap(), &md).unwrap();
        md
    }

    #[test]
    fn lists_refs_sorted_and_reads_created() {
        let (_d, s) = store();
        assert!(s.list_refs().unwrap().is_empty());
        put_image(&s, "zeta:1", &[b"z"]);
        let md = put_image(&s, "alpha:2", &[b"a"]);
        let refs = s.list_refs().unwrap();
        let names: Vec<_> = refs.iter().map(|(r, _)| r.to_string()).collect();
        assert_eq!(names, ["registry-1.docker.io/library/alpha:2", "registry-1.docker.io/library/zeta:1"]);
        assert_eq!(refs[0].1, md);
        let img = s.load_image(&"alpha:2".parse().unwrap()).unwrap();
        assert_eq!(img.config.created.as_deref(), Some("2026-01-02T03:04:05Z"));
    }

    #[test]
    fn remove_deletes_only_blobs_no_other_image_uses() {
        let (_d, s) = store();
        let shared: &[u8] = b"shared-layer";
        put_image(&s, "a:1", &[shared, b"only-a"]);
        put_image(&s, "b:1", &[shared, b"only-b"]);

        let report = s.remove_image(&"a:1".parse().unwrap()).unwrap().unwrap();
        // manifest + config + the one private layer
        assert_eq!(report.blobs_removed, 3);
        assert!(report.bytes_freed >= b"only-a".len() as u64);
        assert!(s.get_ref(&"a:1".parse().unwrap()).unwrap().is_none());
        assert!(!s.contains(&Digest::of(b"only-a")));
        assert!(s.contains(&Digest::of(shared)));
        // b is still complete
        assert!(s.load_image(&"b:1".parse().unwrap()).is_ok());
        assert!(s.contains(&Digest::of(b"only-b")));

        assert_eq!(s.remove_image(&"a:1".parse().unwrap()).unwrap(), None);
        s.remove_image(&"b:1".parse().unwrap()).unwrap().unwrap();
        assert!(!s.contains(&Digest::of(shared)));
    }

    #[test]
    fn two_names_for_one_image_keep_its_blobs() {
        let (_d, s) = store();
        let md = put_image(&s, "a:1", &[b"layer"]);
        s.set_ref(&"a:latest".parse().unwrap(), &md).unwrap();
        let report = s.remove_image(&"a:1".parse().unwrap()).unwrap().unwrap();
        assert_eq!(report.blobs_removed, 0);
        assert!(s.load_image(&"a:latest".parse().unwrap()).is_ok());
    }
}
