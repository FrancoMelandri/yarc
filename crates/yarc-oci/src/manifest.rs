//! OCI / Docker v2 manifest, index and image-config types (pure, no I/O).

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::error::{OciError, Result};

pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// All manifest media types we can handle, for the `Accept` header.
pub const ACCEPTED: [&str; 4] = [OCI_INDEX, OCI_MANIFEST, DOCKER_LIST, DOCKER_MANIFEST];

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    #[serde(default)]
    pub variant: Option<String>,
}

impl Platform {
    pub fn new(os: &str, architecture: &str) -> Self {
        Platform { os: os.into(), architecture: architecture.into(), variant: None }
    }

    /// `linux/<arch of this machine>`; containers always run Linux images.
    pub fn host() -> Self {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            "x86" => "386",
            other => other,
        };
        Platform::new("linux", arch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(default)]
    pub platform: Option<Platform>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ImageManifest {
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ImageIndex {
    pub manifests: Vec<Descriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Manifest {
    Image(ImageManifest),
    Index(ImageIndex),
}

/// Decide between image manifest and index by shape (works for OCI and Docker types).
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest> {
    let json_err = |e: serde_json::Error| OciError::Json { what: "manifest".into(), msg: e.to_string() };
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(json_err)?;
    if value.get("schemaVersion").and_then(|v| v.as_u64()) == Some(1) {
        return Err(OciError::UnsupportedManifest("schema version 1".into()));
    }
    if value.get("manifests").is_some() {
        serde_json::from_value(value).map(Manifest::Index).map_err(json_err)
    } else if value.get("layers").is_some() {
        serde_json::from_value(value).map(Manifest::Image).map_err(json_err)
    } else {
        Err(OciError::UnsupportedManifest("neither 'manifests' nor 'layers' present".into()))
    }
}

/// Pick the entry matching `want` (os + architecture). Attestation entries
/// (`unknown/unknown`) never match.
pub fn select_platform<'a>(index: &'a ImageIndex, want: &Platform) -> Result<&'a Descriptor> {
    index
        .manifests
        .iter()
        .find(|d| {
            d.platform
                .as_ref()
                .is_some_and(|p| p.os == want.os && p.architecture == want.architecture)
        })
        .ok_or_else(|| OciError::NoMatchingPlatform {
            os: want.os.clone(),
            arch: want.architecture.clone(),
        })
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct ImageConfig {
    /// RFC 3339 build time, as written by the image's builder.
    pub created: Option<String>,
    pub architecture: String,
    pub os: String,
    pub config: ContainerConfig,
    pub rootfs: RootFs,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct ContainerConfig {
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub env: Option<Vec<String>>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub exposed_ports: Option<BTreeMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct RootFs {
    #[serde(rename = "type")]
    pub kind: String,
    pub diff_ids: Vec<String>,
}

pub fn parse_config(bytes: &[u8]) -> Result<ImageConfig> {
    serde_json::from_slice(bytes)
        .map_err(|e| OciError::Json { what: "image config".into(), msg: e.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const D2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    const D3: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";

    fn index_json() -> String {
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_INDEX}","manifests":[
              {{"mediaType":"{OCI_MANIFEST}","digest":"{D1}","size":10,"platform":{{"architecture":"amd64","os":"linux"}}}},
              {{"mediaType":"{OCI_MANIFEST}","digest":"{D2}","size":10,"platform":{{"architecture":"arm64","os":"linux","variant":"v8"}}}},
              {{"mediaType":"{OCI_MANIFEST}","digest":"{D3}","size":10,"platform":{{"architecture":"unknown","os":"unknown"}}}}
            ]}}"#
        )
    }

    #[test]
    fn selects_matching_platform_and_ignores_attestations() {
        let Manifest::Index(idx) = parse_manifest(index_json().as_bytes()).unwrap() else {
            panic!("expected index")
        };
        assert_eq!(select_platform(&idx, &Platform::new("linux", "amd64")).unwrap().digest, D1);
        assert_eq!(select_platform(&idx, &Platform::new("linux", "arm64")).unwrap().digest, D2);
        assert!(matches!(
            select_platform(&idx, &Platform::new("linux", "riscv64")),
            Err(OciError::NoMatchingPlatform { .. })
        ));
        assert!(select_platform(&idx, &Platform::new("unknown", "unknown")).is_ok()); // explicit ask only
    }

    #[test]
    fn parses_image_manifest_docker_and_oci() {
        for mt in [DOCKER_MANIFEST, OCI_MANIFEST] {
            let j = format!(
                r#"{{"schemaVersion":2,"mediaType":"{mt}","config":{{"mediaType":"c","digest":"{D1}","size":1}},
                    "layers":[{{"mediaType":"l","digest":"{D2}","size":2}}]}}"#
            );
            let Manifest::Image(m) = parse_manifest(j.as_bytes()).unwrap() else { panic!() };
            assert_eq!(m.config.digest, D1);
            assert_eq!(m.layers.len(), 1);
        }
    }

    #[test]
    fn rejects_schema1_and_unknown_shapes() {
        assert!(matches!(
            parse_manifest(br#"{"schemaVersion":1,"fsLayers":[]}"#),
            Err(OciError::UnsupportedManifest(_))
        ));
        assert!(matches!(parse_manifest(br#"{"schemaVersion":2}"#), Err(OciError::UnsupportedManifest(_))));
        assert!(matches!(parse_manifest(b"not json"), Err(OciError::Json { .. })));
    }

    #[test]
    fn parses_image_config_with_nulls() {
        let j = br#"{"architecture":"amd64","os":"linux",
            "config":{"Env":["PATH=/bin"],"Cmd":["/bin/sh"],"Entrypoint":null,"WorkingDir":"/","User":"",
                      "ExposedPorts":{"80/tcp":{}}},
            "rootfs":{"type":"layers","diff_ids":["sha256:aa"]}}"#;
        let c = parse_config(j).unwrap();
        assert_eq!(c.config.cmd.as_deref(), Some(&["/bin/sh".to_string()][..]));
        assert_eq!(c.config.entrypoint, None);
        assert_eq!(c.config.working_dir.as_deref(), Some("/"));
        assert!(c.config.exposed_ports.unwrap().contains_key("80/tcp"));
        assert_eq!(c.rootfs.diff_ids, vec!["sha256:aa"]);
    }

    #[test]
    fn host_platform_is_linux() {
        assert_eq!(Platform::host().os, "linux");
    }
}
