//! Network tests against Docker Hub. Run with: cargo test -p yarc-oci -- --ignored

use yarc_oci::{pull, BlobStore, Digest, Platform, Reference, RegistryClient, UreqTransport};

fn pull_live(image: &str) {
    let client = RegistryClient::new(UreqTransport::new());
    let reference: Reference = image.parse().unwrap();
    let resolved = client.resolve(&reference, &Platform::host()).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::open(dir.path()).unwrap();

    let first = pull(&client, &store, &resolved, 4).unwrap();
    println!("{image}: {first:?}");
    assert!(first.downloaded > 0 && first.bytes_downloaded > 0);
    for layer in &resolved.manifest.layers {
        assert!(store.contains(&Digest::parse(&layer.digest).unwrap()));
    }
    assert_eq!(store.get_ref(&reference).unwrap(), Some(resolved.manifest_digest.clone()));

    // Everything is cached now.
    let second = pull(&client, &store, &resolved, 4).unwrap();
    assert_eq!(second.downloaded, 0);
    assert_eq!(second.skipped, first.downloaded);
}

#[test]
#[ignore = "needs network access"]
fn pulls_single_layer_image() {
    pull_live("alpine:3.20");
}

#[test]
#[ignore = "needs network access"]
fn pulls_multi_layer_image_in_parallel() {
    pull_live("nginx:alpine-slim");
}

#[test]
#[ignore = "needs network access"]
fn resolves_alpine_from_docker_hub() {
    let client = RegistryClient::new(UreqTransport::new());
    let reference: Reference = "alpine:3.20".parse().unwrap();
    let image = client.resolve(&reference, &Platform::host()).unwrap();

    assert!(!image.manifest.layers.is_empty());
    assert_eq!(image.config.os, "linux");
    assert_eq!(image.config.architecture, Platform::host().architecture);
    assert_eq!(image.config.rootfs.diff_ids.len(), image.manifest.layers.len());
    println!("{} -> {}", reference, image.manifest_digest);
}
