//! Network tests against Docker Hub. Run with: cargo test -p yarc-oci -- --ignored

use yarc_oci::{Platform, Reference, RegistryClient, UreqTransport};

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
