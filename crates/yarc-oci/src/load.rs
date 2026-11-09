//! Import images from an OCI image layout (what `docker save` writes with the
//! containerd image store, `skopeo copy oci:`, `crane pull --format oci`...).
//!
//! The layout is a directory, or a tar of it:
//! ```text
//! oci-layout
//! index.json                 manifests, each named by an annotation
//! blobs/sha256/<hex>         manifests, configs and layers
//! ```
//! Every blob is verified against the digest in its file name while it is copied
//! into the store, so a damaged archive is rejected. The archive is read once, in
//! order, and never extracted to disk: entry names are only matched against the
//! fixed layout names, so nothing in it can choose where a file is written.

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use serde::Deserialize;

use crate::digest::Digest;
use crate::error::{OciError, Result};
use crate::manifest::{parse_manifest, Manifest};
use crate::reference::Reference;
use crate::store::{io_err, BlobStore};

/// Annotation containerd/Docker use for the full image name (`docker.io/library/x:1`).
const NAME_ANNOTATION: &str = "io.containerd.image.name";
/// OCI standard annotation; usually only the tag, so only used when it is a full name.
const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedImage {
    pub reference: Reference,
    pub manifest_digest: Digest,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadReport {
    /// Images that now resolve by name.
    pub images: Vec<LoadedImage>,
    pub blobs_added: usize,
    pub blobs_present: usize,
    pub bytes_added: u64,
    /// Manifests in the layout that carry no usable full image name.
    pub unnamed: Vec<Digest>,
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<IndexEntry>,
}

#[derive(Deserialize)]
struct IndexEntry {
    digest: String,
    #[serde(default)]
    annotations: std::collections::BTreeMap<String, String>,
}

/// Import a layout from `path`: a directory, or a tar file.
pub fn load_path(store: &BlobStore, path: &Path) -> Result<LoadReport> {
    if path.is_dir() {
        load_dir(store, path)
    } else {
        let file = File::open(path).map_err(io_err(path))?;
        load_archive(store, std::io::BufReader::with_capacity(1 << 20, file))
    }
}

/// Import a layout from a tar stream.
pub fn load_archive(store: &BlobStore, reader: impl Read) -> Result<LoadReport> {
    let mut report = LoadReport::default();
    let mut index: Option<Vec<u8>> = None;
    let mut docker_format = false;

    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().map_err(|e| OciError::BadLayer(format!("not a tar archive: {e}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| OciError::BadLayer(format!("cannot read the archive: {e}")))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let name = entry.path().map_err(|e| OciError::BadLayer(e.to_string()))?.to_string_lossy().into_owned();
        let name = name.trim_start_matches("./");
        if name == "index.json" {
            index = Some(read_small(&mut entry, "index.json")?);
        } else if name == "manifest.json" {
            docker_format = true;
        } else if let Some(hex) = name.strip_prefix("blobs/sha256/") {
            let digest = Digest::parse(&format!("sha256:{hex}"))?;
            let size = entry.size();
            add_blob(store, &digest, &mut entry, size, &mut report)?;
        }
    }

    let index = index.ok_or_else(|| {
        if docker_format {
            OciError::Unsupported(
                "this is a legacy `docker save` archive (manifest.json), not an OCI layout (index.json)".into(),
            )
        } else {
            OciError::BadLayer("no index.json: not an OCI image layout".into())
        }
    })?;
    finish(store, &index, &mut report)?;
    Ok(report)
}

/// Import a layout from a directory.
pub fn load_dir(store: &BlobStore, dir: &Path) -> Result<LoadReport> {
    let mut report = LoadReport::default();
    let index_path = dir.join("index.json");
    let index = fs::read(&index_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            OciError::BadLayer(format!("{} has no index.json: not an OCI image layout", dir.display()))
        } else {
            io_err(&index_path)(e)
        }
    })?;

    let blobs = dir.join("blobs").join("sha256");
    for entry in fs::read_dir(&blobs).map_err(io_err(&blobs))? {
        let entry = entry.map_err(io_err(&blobs))?;
        let path = entry.path();
        let meta = fs::metadata(&path).map_err(io_err(&path))?;
        if !meta.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
        let digest = Digest::parse(&format!("sha256:{name}"))?;
        let mut file = File::open(&path).map_err(io_err(&path))?;
        add_blob(store, &digest, &mut file, meta.len(), &mut report)?;
    }
    finish(store, &index, &mut report)?;
    Ok(report)
}

fn add_blob(store: &BlobStore, digest: &Digest, reader: &mut impl Read, size: u64, report: &mut LoadReport) -> Result<()> {
    if store.contains(digest) {
        report.blobs_present += 1;
        return Ok(());
    }
    store.put_stream(digest, reader, Some(size))?;
    report.blobs_added += 1;
    report.bytes_added += size;
    Ok(())
}

/// `index.json` and friends are small; refuse anything absurd.
fn read_small(reader: &mut impl Read, what: &str) -> Result<Vec<u8>> {
    const LIMIT: u64 = 16 * 1024 * 1024;
    let mut buf = Vec::new();
    reader
        .take(LIMIT + 1)
        .read_to_end(&mut buf)
        .map_err(|e| OciError::BadLayer(format!("cannot read {what}: {e}")))?;
    if buf.len() as u64 > LIMIT {
        return Err(OciError::TooLarge(what.into(), LIMIT));
    }
    Ok(buf)
}

/// Check every named image is complete, then publish the names.
fn finish(store: &BlobStore, index: &[u8], report: &mut LoadReport) -> Result<()> {
    let index: Index = serde_json::from_slice(index)
        .map_err(|e| OciError::Json { what: "index.json".into(), msg: e.to_string() })?;

    // Verify everything first so a broken layout publishes no names at all.
    let mut named = Vec::new();
    for entry in &index.manifests {
        let digest = Digest::parse(&entry.digest)?;
        if !store.contains(&digest) {
            return Err(OciError::BadLayer(format!("manifest {digest} is listed in index.json but missing from blobs/")));
        }
        let image = match parse_manifest(&store.read_blob(&digest)?)? {
            Manifest::Image(m) => m,
            Manifest::Index(_) => {
                return Err(OciError::Unsupported(format!(
                    "{digest} is a multi-platform index; export a single-platform image"
                )))
            }
        };
        for d in std::iter::once(&image.config).chain(image.layers.iter()) {
            let blob = Digest::parse(&d.digest)?;
            if !store.contains(&blob) {
                return Err(OciError::BadLayer(format!("image {digest} needs {blob}, which is missing from blobs/")));
            }
        }
        match image_name(&entry.annotations) {
            Some(reference) => named.push(LoadedImage { reference, manifest_digest: digest }),
            None => report.unnamed.push(digest),
        }
    }
    for image in &named {
        store.set_ref(&image.reference, &image.manifest_digest)?;
    }
    report.images = named;
    Ok(())
}

fn image_name(annotations: &std::collections::BTreeMap<String, String>) -> Option<Reference> {
    let full = annotations.get(NAME_ANNOTATION).or_else(|| {
        annotations.get(REF_NAME_ANNOTATION).filter(|n| n.contains('/'))
    })?;
    full.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Layout {
        blobs: Vec<(Digest, Vec<u8>)>,
        index: String,
    }

    /// One image (config + one layer) named `name`, or unnamed when `None`.
    fn layout(name: Option<&str>) -> Layout {
        let layer = b"layer-bytes".to_vec();
        let config = br#"{"architecture":"amd64","os":"linux","config":{},"rootfs":{"type":"layers","diff_ids":[]}}"#.to_vec();
        let (ld, cd) = (Digest::of(&layer), Digest::of(&config));
        let manifest = format!(
            r#"{{"schemaVersion":2,"config":{{"mediaType":"c","digest":"{cd}","size":{}}},"layers":[{{"mediaType":"l","digest":"{ld}","size":{}}}]}}"#,
            config.len(),
            layer.len()
        )
        .into_bytes();
        let md = Digest::of(&manifest);
        let annotations = match name {
            Some(n) => format!(r#","annotations":{{"{NAME_ANNOTATION}":"{n}","{REF_NAME_ANNOTATION}":"1"}}"#),
            None => String::new(),
        };
        let index = format!(
            r#"{{"schemaVersion":2,"manifests":[{{"mediaType":"m","digest":"{md}","size":{}{annotations}}}]}}"#,
            manifest.len()
        );
        Layout { blobs: vec![(ld, layer), (cd, config), (md, manifest)], index }
    }

    fn tar_of(l: &Layout, skip_blob: Option<usize>, extra: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let add = |b: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            // Raw name: `Builder::append_data` refuses `..`, which one test needs.
            h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
            h.set_cksum();
            b.append(&h, data).unwrap();
        };
        for (i, (d, bytes)) in l.blobs.iter().enumerate() {
            if skip_blob != Some(i) {
                add(&mut b, &format!("blobs/sha256/{}", d.hex()), bytes);
            }
        }
        add(&mut b, "index.json", l.index.as_bytes());
        for (p, d) in extra {
            add(&mut b, p, d);
        }
        b.into_inner().unwrap()
    }

    fn store() -> (TempDir, BlobStore) {
        let dir = TempDir::new().unwrap();
        let s = BlobStore::open(dir.path()).unwrap();
        (dir, s)
    }

    #[test]
    fn loads_an_archive_and_the_image_resolves_by_name() {
        let (_d, s) = store();
        let l = layout(Some("docker.io/library/app:1.0"));
        let report = load_archive(&s, &tar_of(&l, None, &[])[..]).unwrap();
        assert_eq!((report.blobs_added, report.blobs_present), (3, 0));
        assert_eq!(report.images.len(), 1);
        let r: Reference = "app:1.0".parse().unwrap();
        let img = s.load_image(&r).unwrap();
        assert_eq!(img.manifest_digest, l.blobs[2].0);
    }

    #[test]
    fn loading_twice_is_a_no_op() {
        let (_d, s) = store();
        let tar = tar_of(&layout(Some("app:1.0")), None, &[]);
        load_archive(&s, &tar[..]).unwrap();
        let again = load_archive(&s, &tar[..]).unwrap();
        assert_eq!((again.blobs_added, again.blobs_present), (0, 3));
        assert_eq!(again.images.len(), 1);
    }

    #[test]
    fn index_may_come_before_the_blobs_and_dot_slash_names_work() {
        let (_d, s) = store();
        let l = layout(Some("app:1.0"));
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(l.index.len() as u64);
        h.set_mode(0o644);
        b.append_data(&mut h, "./index.json", l.index.as_bytes()).unwrap();
        for (d, bytes) in &l.blobs {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o644);
            b.append_data(&mut h, format!("./blobs/sha256/{}", d.hex()), &bytes[..]).unwrap();
        }
        load_archive(&s, &b.into_inner().unwrap()[..]).unwrap();
        assert!(s.get_ref(&"app:1.0".parse().unwrap()).unwrap().is_some());
    }

    #[test]
    fn a_blob_that_does_not_match_its_name_is_rejected_and_nothing_is_named() {
        let (_d, s) = store();
        let mut l = layout(Some("app:1.0"));
        l.blobs[0].1 = b"tampered".to_vec();
        let err = load_archive(&s, &tar_of(&l, None, &[])[..]).unwrap_err();
        assert!(matches!(err, OciError::DigestMismatch { .. } | OciError::SizeMismatch { .. }), "{err}");
        assert!(s.get_ref(&"app:1.0".parse().unwrap()).unwrap().is_none());
    }

    #[test]
    fn a_missing_layer_is_reported_and_nothing_is_named() {
        let (_d, s) = store();
        let err = load_archive(&s, &tar_of(&layout(Some("app:1.0")), Some(0), &[])[..]).unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
        assert!(s.get_ref(&"app:1.0".parse().unwrap()).unwrap().is_none());
    }

    #[test]
    fn unnamed_manifests_are_reported_not_named() {
        let (_d, s) = store();
        let report = load_archive(&s, &tar_of(&layout(None), None, &[])[..]).unwrap();
        assert!(report.images.is_empty());
        assert_eq!(report.unnamed.len(), 1);
    }

    #[test]
    fn a_bare_tag_annotation_is_not_a_name() {
        let a = std::collections::BTreeMap::from([(REF_NAME_ANNOTATION.to_string(), "1.0".to_string())]);
        assert!(image_name(&a).is_none());
        let a = std::collections::BTreeMap::from([(REF_NAME_ANNOTATION.to_string(), "ghcr.io/o/app:1.0".to_string())]);
        assert_eq!(image_name(&a).unwrap().to_string(), "ghcr.io/o/app:1.0");
    }

    #[test]
    fn legacy_docker_save_and_random_files_get_clear_errors() {
        let (_d, s) = store();
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(2);
        h.set_mode(0o644);
        b.append_data(&mut h, "manifest.json", &b"[]"[..]).unwrap();
        let err = load_archive(&s, &b.into_inner().unwrap()[..]).unwrap_err();
        assert!(err.to_string().contains("legacy `docker save`"), "{err}");

        let err = load_archive(&s, &b"this is not a tar archive at all, not even close........"[..]).unwrap_err();
        assert!(matches!(err, OciError::BadLayer(_)), "{err}");
    }

    #[test]
    fn entries_cannot_escape_the_store() {
        let (d, s) = store();
        let l = layout(Some("app:1.0"));
        // Odd names are simply ignored; only the fixed layout names are acted on.
        let tar = tar_of(&l, None, &[("../../evil", b"x"), ("blobs/sha256/../../evil2", b"x")]);
        load_archive(&s, &tar[..]).map(|_| ()).or_else(|e| match e {
            OciError::InvalidDigest(_) => Ok(()),
            e => Err(e),
        })
        .unwrap();
        assert!(!d.path().parent().unwrap().join("evil").exists());
        assert!(!d.path().join("evil2").exists());
    }

    #[test]
    fn loads_a_directory() {
        let (_d, s) = store();
        let src = TempDir::new().unwrap();
        let l = layout(Some("app:2.0"));
        fs::create_dir_all(src.path().join("blobs/sha256")).unwrap();
        fs::write(src.path().join("index.json"), &l.index).unwrap();
        for (d, bytes) in &l.blobs {
            fs::write(src.path().join("blobs/sha256").join(d.hex()), bytes).unwrap();
        }
        let report = load_path(&s, src.path()).unwrap();
        assert_eq!(report.images.len(), 1);
        assert!(s.get_ref(&"app:2.0".parse().unwrap()).unwrap().is_some());

        let empty = TempDir::new().unwrap();
        assert!(load_path(&s, empty.path()).is_err());
    }
}
