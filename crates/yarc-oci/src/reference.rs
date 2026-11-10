use std::fmt;
use std::str::FromStr;

use crate::digest::Digest;
use crate::error::{OciError, Result};

pub const DOCKER_HUB_REGISTRY: &str = "registry-1.docker.io";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Tag(String),
    Digest(Digest),
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Tag(t) => f.write_str(t),
            Target::Digest(d) => write!(f, "{d}"),
        }
    }
}

/// A fully-resolved image reference: registry host, repository path, tag or digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub registry: String,
    pub repository: String,
    pub target: Target,
}

impl Reference {
    /// `http` for local registries, `https` otherwise.
    pub fn scheme(&self) -> &'static str {
        let host = self.registry.split(':').next().unwrap_or("");
        if host == "localhost" || host == "127.0.0.1" {
            "http"
        } else {
            "https"
        }
    }

    pub fn manifest_url(&self, reference: &str) -> String {
        format!("{}://{}/v2/{}/manifests/{}", self.scheme(), self.registry, self.repository, reference)
    }

    pub fn blob_url(&self, digest: &Digest) -> String {
        format!("{}://{}/v2/{}/blobs/{}", self.scheme(), self.registry, self.repository, digest)
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.target {
            Target::Tag(t) => write!(f, "{}/{}:{}", self.registry, self.repository, t),
            Target::Digest(d) => write!(f, "{}/{}@{}", self.registry, self.repository, d),
        }
    }
}

impl FromStr for Reference {
    type Err = OciError;

    fn from_str(input: &str) -> Result<Self> {
        let err = |msg: &str| OciError::Reference(input.into(), msg.into());
        if input.is_empty() {
            return Err(err("empty"));
        }

        // name[:tag][@digest]
        let (name_tag, digest) = match input.split_once('@') {
            Some((n, d)) => (n, Some(Digest::parse(d)?)),
            None => (input, None),
        };
        let last_slash = name_tag.rfind('/').map(|i| i + 1).unwrap_or(0);
        let (name, tag) = match name_tag[last_slash..].rfind(':') {
            Some(i) => {
                let colon = last_slash + i;
                (&name_tag[..colon], Some(&name_tag[colon + 1..]))
            }
            None => (name_tag, None),
        };

        // registry / repository
        let (registry, repository) = match name.split_once('/') {
            Some((first, rest)) if first.contains('.') || first.contains(':') || first == "localhost" => {
                (first.to_string(), rest.to_string())
            }
            _ => (DOCKER_HUB_REGISTRY.to_string(), name.to_string()),
        };
        let (registry, repository) =
            if matches!(registry.as_str(), "docker.io" | "index.docker.io" | DOCKER_HUB_REGISTRY) {
                let repo = if repository.contains('/') { repository } else { format!("library/{repository}") };
                (DOCKER_HUB_REGISTRY.to_string(), repo)
            } else {
                (registry, repository)
            };

        validate_repository(&repository).map_err(err)?;

        let target = match (digest, tag) {
            (Some(d), _) => Target::Digest(d),
            (None, Some(t)) => {
                validate_tag(t).map_err(err)?;
                Target::Tag(t.into())
            }
            (None, None) => Target::Tag("latest".into()),
        };
        Ok(Reference { registry, repository, target })
    }
}

fn validate_repository(repo: &str) -> std::result::Result<(), &'static str> {
    if repo.is_empty() {
        return Err("empty repository");
    }
    for part in repo.split('/') {
        let ok = !part.is_empty()
            && part.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
            && part.starts_with(|c: char| c.is_ascii_alphanumeric());
        if !ok {
            return Err("repository must be lowercase alphanumerics separated by '.', '_', '-' or '/'");
        }
    }
    Ok(())
}

fn validate_tag(tag: &str) -> std::result::Result<(), &'static str> {
    let ok = !tag.is_empty()
        && tag.len() <= 128
        && tag.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        && tag.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if ok {
        Ok(())
    } else {
        Err("invalid tag")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn r(s: &str) -> Reference {
        s.parse().unwrap()
    }

    fn tag(reg: &str, repo: &str, t: &str) -> Reference {
        Reference { registry: reg.into(), repository: repo.into(), target: Target::Tag(t.into()) }
    }

    #[test]
    fn docker_hub_defaults() {
        assert_eq!(r("alpine"), tag(DOCKER_HUB_REGISTRY, "library/alpine", "latest"));
        assert_eq!(r("postgres:16"), tag(DOCKER_HUB_REGISTRY, "library/postgres", "16"));
        assert_eq!(r("myorg/api:1.2"), tag(DOCKER_HUB_REGISTRY, "myorg/api", "1.2"));
        assert_eq!(r("docker.io/alpine:3"), tag(DOCKER_HUB_REGISTRY, "library/alpine", "3"));
        assert_eq!(r("index.docker.io/myorg/api"), tag(DOCKER_HUB_REGISTRY, "myorg/api", "latest"));
    }

    #[test]
    fn custom_registries() {
        assert_eq!(r("ghcr.io/org/app:1.2"), tag("ghcr.io", "org/app", "1.2"));
        assert_eq!(r("localhost:5000/foo"), tag("localhost:5000", "foo", "latest"));
        assert_eq!(r("reg.example.com:8443/a/b/c:v1"), tag("reg.example.com:8443", "a/b/c", "v1"));
    }

    #[test]
    fn digests() {
        let x = r(&format!("alpine@{D}"));
        assert_eq!(x.target, Target::Digest(Digest::parse(D).unwrap()));
        // digest wins over tag
        let y = r(&format!("ghcr.io/o/a:1.0@{D}"));
        assert_eq!(y.target, Target::Digest(Digest::parse(D).unwrap()));
        assert_eq!(y.repository, "o/a");
    }

    #[test]
    fn rejects_invalid() {
        for bad in ["", "Alpine", "a//b", "alpine:", "alpine:bad tag", "alpine@sha256:123", "/alpine", "a/B"] {
            assert!(bad.parse::<Reference>().is_err(), "{bad}");
        }
    }

    #[test]
    fn urls_and_scheme() {
        let x = r("alpine");
        assert_eq!(x.manifest_url("latest"), "https://registry-1.docker.io/v2/library/alpine/manifests/latest");
        assert_eq!(r("localhost:5000/foo").scheme(), "http");
        assert_eq!(r("ghcr.io/o/a").scheme(), "https");
    }

    #[test]
    fn display_is_canonical() {
        assert_eq!(r("alpine").to_string(), "registry-1.docker.io/library/alpine:latest");
    }
}
