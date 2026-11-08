//! Pull an already-resolved image into a [`BlobStore`].

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;

use crate::client::{RegistryClient, ResolvedImage};
use crate::digest::Digest;
use crate::error::Result;
use crate::manifest::Descriptor;
use crate::reference::Reference;
use crate::store::BlobStore;
use crate::transport::Transport;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PullReport {
    /// Distinct layers fetched from the registry.
    pub downloaded: usize,
    /// Distinct layers that were already in the store.
    pub skipped: usize,
    pub bytes_downloaded: u64,
}

enum Outcome {
    Downloaded(u64),
    Skipped,
}

/// Store the manifest, config and all layers of `image`, using up to `jobs` threads.
///
/// The reference is only recorded once everything was stored and verified, so a
/// recorded ref always points at a complete image.
pub fn pull<T: Transport + Sync>(
    client: &RegistryClient<T>,
    store: &BlobStore,
    image: &ResolvedImage,
    jobs: usize,
) -> Result<PullReport> {
    store.put_bytes(&image.config_digest, &image.config_bytes)?;
    store.put_bytes(&image.manifest_digest, &image.manifest_bytes)?;

    let mut seen = HashSet::new();
    let layers: Vec<&Descriptor> = image
        .manifest
        .layers
        .iter()
        .filter(|l| seen.insert(l.digest.as_str()))
        .collect();

    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let results: Mutex<Vec<Result<Outcome>>> = Mutex::new(Vec::new());
    let workers = jobs.clamp(1, layers.len().max(1));

    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if failed.load(Ordering::Relaxed) {
                    break;
                }
                let Some(layer) = layers.get(next.fetch_add(1, Ordering::Relaxed)) else {
                    break;
                };
                let outcome = fetch_layer(client, store, &image.reference, layer);
                if outcome.is_err() {
                    failed.store(true, Ordering::Relaxed);
                }
                results.lock().unwrap().push(outcome);
            });
        }
    });

    let mut report = PullReport::default();
    for outcome in results.into_inner().unwrap() {
        match outcome? {
            Outcome::Downloaded(n) => {
                report.downloaded += 1;
                report.bytes_downloaded += n;
            }
            Outcome::Skipped => report.skipped += 1,
        }
    }

    store.set_ref(&image.reference, &image.manifest_digest)?;
    Ok(report)
}

fn fetch_layer<T: Transport>(
    client: &RegistryClient<T>,
    store: &BlobStore,
    reference: &Reference,
    layer: &Descriptor,
) -> Result<Outcome> {
    let digest = Digest::parse(&layer.digest)?;
    if store.contains(&digest) {
        return Ok(Outcome::Skipped);
    }
    let response = client.fetch_blob(reference, &digest)?;
    let written = store.put_stream(&digest, response.body, Some(layer.size))?;
    Ok(Outcome::Downloaded(written))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::error::OciError;
    use crate::manifest::{Platform, DOCKER_MANIFEST};
    use crate::testutil::Fake;

    const BASE: &str = "https://registry-1.docker.io/v2/library/alpine";

    struct Setup {
        fake: Arc<Fake>,
        client: RegistryClient<Arc<Fake>>,
        image: ResolvedImage,
        layer_digests: Vec<Digest>,
        _dir: tempfile::TempDir,
        store: BlobStore,
    }

    fn layer_bytes(i: usize) -> Vec<u8> {
        format!("layer-{i}-").repeat(1000).into_bytes()
    }

    /// Registry serving alpine:latest with the given layers; resolved through the real client.
    fn setup(layers: &[Vec<u8>]) -> Setup {
        let config = br#"{"architecture":"amd64","os":"linux"}"#;
        let config_digest = Digest::of(config);
        let layer_digests: Vec<Digest> = layers.iter().map(|l| Digest::of(l)).collect();
        let layer_json: Vec<String> = layers
            .iter()
            .zip(&layer_digests)
            .map(|(l, d)| format!(r#"{{"mediaType":"l","digest":"{d}","size":{}}}"#, l.len()))
            .collect();
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{DOCKER_MANIFEST}","config":{{"mediaType":"c","digest":"{config_digest}","size":{}}},"layers":[{}]}}"#,
            config.len(),
            layer_json.join(",")
        );

        let fake = Arc::new(Fake::default());
        fake.route(&format!("{BASE}/manifests/latest"), 200, &[], manifest);
        fake.route(&format!("{BASE}/blobs/{config_digest}"), 200, &[], config.to_vec());
        for (l, d) in layers.iter().zip(&layer_digests) {
            fake.route(&format!("{BASE}/blobs/{d}"), 200, &[], l.clone());
        }

        let client = RegistryClient::new(fake.clone());
        let reference: Reference = "alpine".parse().unwrap();
        let image = client.resolve(&reference, &Platform::host()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path()).unwrap();
        Setup { fake, client, image, layer_digests, _dir: dir, store }
    }

    fn layer_url(d: &Digest) -> String {
        format!("{BASE}/blobs/{d}")
    }

    #[test]
    fn pulls_everything_into_the_store() {
        let layers = vec![layer_bytes(0), layer_bytes(1)];
        let s = setup(&layers);

        let report = pull(&s.client, &s.store, &s.image, 4).unwrap();

        assert_eq!(report.downloaded, 2);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.bytes_downloaded, layers.iter().map(|l| l.len() as u64).sum::<u64>());
        for (l, d) in layers.iter().zip(&s.layer_digests) {
            assert_eq!(&s.store.read_blob(d).unwrap(), l);
        }
        assert_eq!(s.store.read_blob(&s.image.config_digest).unwrap(), s.image.config_bytes);
        assert_eq!(s.store.read_blob(&s.image.manifest_digest).unwrap(), s.image.manifest_bytes);
        assert_eq!(s.store.get_ref(&s.image.reference).unwrap(), Some(s.image.manifest_digest.clone()));
    }

    #[test]
    fn second_pull_skips_layers_already_present() {
        let s = setup(&[layer_bytes(0), layer_bytes(1)]);
        pull(&s.client, &s.store, &s.image, 2).unwrap();
        let before: Vec<usize> = s.layer_digests.iter().map(|d| s.fake.count_calls(&layer_url(d))).collect();

        let report = pull(&s.client, &s.store, &s.image, 2).unwrap();

        assert_eq!(report, PullReport { downloaded: 0, skipped: 2, bytes_downloaded: 0 });
        let after: Vec<usize> = s.layer_digests.iter().map(|d| s.fake.count_calls(&layer_url(d))).collect();
        assert_eq!(before, after, "no layer was requested again");
    }

    #[test]
    fn duplicate_layers_are_fetched_once() {
        let l = layer_bytes(7);
        let s = setup(&[l.clone(), layer_bytes(8), l]);
        let report = pull(&s.client, &s.store, &s.image, 3).unwrap();
        assert_eq!(report.downloaded, 2);
        assert_eq!(s.fake.count_calls(&layer_url(&s.layer_digests[0])), 1);
    }

    #[test]
    fn many_layers_with_fewer_workers() {
        let layers: Vec<Vec<u8>> = (0..12).map(layer_bytes).collect();
        let s = setup(&layers);
        let report = pull(&s.client, &s.store, &s.image, 3).unwrap();
        assert_eq!(report.downloaded, 12);
        assert!(s.layer_digests.iter().all(|d| s.store.contains(d)));
    }

    #[test]
    fn corrupt_layer_is_rejected_and_ref_not_recorded() {
        let s = setup(&[layer_bytes(0), layer_bytes(1)]);
        let mut corrupt = layer_bytes(1);
        corrupt[0] ^= 0xff; // same length, different content
        s.fake.replace(&layer_url(&s.layer_digests[1]), 200, corrupt);

        let err = pull(&s.client, &s.store, &s.image, 1).unwrap_err();

        assert!(matches!(err, OciError::DigestMismatch { .. }), "{err}");
        assert!(!s.store.contains(&s.layer_digests[1]));
        assert_eq!(s.store.get_ref(&s.image.reference).unwrap(), None);
    }

    #[test]
    fn wrong_layer_size_is_rejected() {
        let s = setup(&[layer_bytes(0)]);
        let mut longer = layer_bytes(0);
        longer.extend_from_slice(b"extra");
        s.fake.replace(&layer_url(&s.layer_digests[0]), 200, longer);

        let err = pull(&s.client, &s.store, &s.image, 1).unwrap_err();

        assert!(matches!(err, OciError::SizeMismatch { .. }), "{err}");
        assert!(!s.store.contains(&s.layer_digests[0]));
    }

    #[test]
    fn registry_error_on_layer_fails_the_pull() {
        let s = setup(&[layer_bytes(0)]);
        s.fake.replace(&layer_url(&s.layer_digests[0]), 500, "");
        let err = pull(&s.client, &s.store, &s.image, 1).unwrap_err();
        assert!(matches!(err, OciError::Http { status: 500, .. }), "{err}");
    }
}
