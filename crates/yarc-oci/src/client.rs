//! Registry client: authentication + manifest/config resolution.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::auth::{parse_challenge, parse_token_response, token_url, BearerChallenge};
use crate::digest::Digest;
use crate::error::{OciError, Result};
use crate::manifest::{
    parse_config, parse_manifest, select_platform, ImageConfig, ImageManifest, Manifest, Platform,
    ACCEPTED,
};
use crate::reference::{Reference, Target};
use crate::transport::{Response, Transport};

/// Upper bound for manifests, indexes, configs and token responses.
const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ResolvedImage {
    pub reference: Reference,
    /// Digest of the platform-specific image manifest.
    pub manifest_digest: Digest,
    pub manifest: ImageManifest,
    pub config_digest: Digest,
    pub config: ImageConfig,
}

pub struct RegistryClient<T: Transport> {
    transport: T,
    /// Bearer tokens keyed by `registry/repository`.
    tokens: Mutex<HashMap<String, String>>,
}

impl<T: Transport> RegistryClient<T> {
    pub fn new(transport: T) -> Self {
        RegistryClient { transport, tokens: Mutex::new(HashMap::new()) }
    }

    /// Resolve a reference to a single-platform manifest and its image config.
    pub fn resolve(&self, reference: &Reference, platform: &Platform) -> Result<ResolvedImage> {
        let top_bytes = self.fetch_manifest(reference, &reference.target.to_string())?;
        let top_digest = match &reference.target {
            Target::Digest(d) => {
                d.verify(&top_bytes, "manifest")?;
                d.clone()
            }
            Target::Tag(_) => Digest::of(&top_bytes),
        };

        let (manifest_digest, manifest) = match parse_manifest(&top_bytes)? {
            Manifest::Image(m) => (top_digest, m),
            Manifest::Index(index) => {
                let chosen = select_platform(&index, platform)?;
                let child_digest = Digest::parse(&chosen.digest)?;
                let child_bytes = self.fetch_manifest(reference, &chosen.digest)?;
                child_digest.verify(&child_bytes, "platform manifest")?;
                match parse_manifest(&child_bytes)? {
                    Manifest::Image(m) => (child_digest, m),
                    Manifest::Index(_) => {
                        return Err(OciError::UnsupportedManifest("nested image index".into()))
                    }
                }
            }
        };

        let config_digest = Digest::parse(&manifest.config.digest)?;
        let resp = self.get(reference, &reference.blob_url(&config_digest), &[])?;
        let config_bytes = resp.read_limited(MAX_JSON_BYTES, "image config")?;
        config_digest.verify(&config_bytes, "image config")?;
        let config = parse_config(&config_bytes)?;

        Ok(ResolvedImage {
            reference: reference.clone(),
            manifest_digest,
            manifest,
            config_digest,
            config,
        })
    }

    fn fetch_manifest(&self, reference: &Reference, ref_or_digest: &str) -> Result<Vec<u8>> {
        let resp = self.get(reference, &reference.manifest_url(ref_or_digest), &ACCEPTED)?;
        resp.read_limited(MAX_JSON_BYTES, "manifest")
    }

    /// GET with bearer-token handling: on 401, fetch a token once and retry.
    fn get(&self, reference: &Reference, url: &str, accept: &[&str]) -> Result<Response> {
        let key = format!("{}/{}", reference.registry, reference.repository);
        let accept_value = accept.join(", ");
        let mut retried = false;
        loop {
            let auth_value = self
                .tokens
                .lock()
                .unwrap()
                .get(&key)
                .map(|t| format!("Bearer {t}"));
            let mut headers: Vec<(&str, &str)> = Vec::new();
            if !accept_value.is_empty() {
                headers.push(("Accept", &accept_value));
            }
            if let Some(a) = &auth_value {
                headers.push(("Authorization", a));
            }

            let resp = self.transport.get(url, &headers)?;
            if resp.status == 401 && !retried {
                let challenge = resp
                    .header("www-authenticate")
                    .ok_or_else(|| OciError::Auth("401 without WWW-Authenticate header".into()))
                    .and_then(parse_challenge)?;
                let token = self.fetch_token(&challenge, reference)?;
                self.tokens.lock().unwrap().insert(key.clone(), token);
                retried = true;
                continue;
            }
            if !(200..300).contains(&resp.status) {
                return Err(OciError::Http { status: resp.status, url: url.into() });
            }
            return Ok(resp);
        }
    }

    fn fetch_token(&self, challenge: &BearerChallenge, reference: &Reference) -> Result<String> {
        let default_scope = format!("repository:{}:pull", reference.repository);
        let url = token_url(challenge, &default_scope);
        let resp = self.transport.get(&url, &[])?;
        if resp.status != 200 {
            return Err(OciError::Auth(format!("token endpoint returned HTTP {}", resp.status)));
        }
        parse_token_response(&resp.read_limited(MAX_JSON_BYTES, "token response")?)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::manifest::{DOCKER_MANIFEST, OCI_INDEX, OCI_MANIFEST};

    type Canned = (u16, Vec<(String, String)>, Vec<u8>);

    /// Scripted transport: per-URL response queue (the last response repeats).
    #[derive(Default)]
    struct Fake {
        routes: Mutex<HashMap<String, VecDeque<Canned>>>,
        calls: Mutex<Vec<(String, Vec<(String, String)>)>>,
    }

    impl Fake {
        fn route(&self, url: &str, status: u16, headers: &[(&str, &str)], body: impl Into<Vec<u8>>) {
            let h = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            self.routes
                .lock()
                .unwrap()
                .entry(url.into())
                .or_default()
                .push_back((status, h, body.into()));
        }

        fn auth_headers_for(&self, url_part: &str) -> Vec<Option<String>> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(u, _)| u.contains(url_part))
                .map(|(_, h)| h.iter().find(|(k, _)| k == "Authorization").map(|(_, v)| v.clone()))
                .collect()
        }
    }

    impl Transport for Fake {
        fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response> {
            self.calls.lock().unwrap().push((
                url.into(),
                headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ));
            let mut routes = self.routes.lock().unwrap();
            let q = routes
                .get_mut(url)
                .unwrap_or_else(|| panic!("unexpected request: {url}"));
            let (status, headers, body) =
                if q.len() > 1 { q.pop_front().unwrap() } else { q.front().unwrap().clone() };
            Ok(Response { status, headers, body: Box::new(Cursor::new(body)) })
        }
    }

    const BASE: &str = "https://registry-1.docker.io/v2/library/alpine";
    const CHALLENGE: &str = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io""#;
    const TOKEN_URL: &str = "https://auth.docker.io/token?service=registry.docker.io&scope=repository%3Alibrary%2Falpine%3Apull";

    struct Fixture {
        fake: Arc<Fake>,
        index_digest: Digest,
        child_digest: Digest,
        config_digest: Digest,
    }

    /// alpine:latest -> index(amd64, arm64) -> image manifest -> config
    fn fixture(child_body_override: Option<&[u8]>) -> Fixture {
        let config = br#"{"architecture":"amd64","os":"linux","config":{"Cmd":["/bin/sh"]},"rootfs":{"type":"layers","diff_ids":[]}}"#;
        let config_digest = Digest::of(config);
        let child = format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST}","config":{{"mediaType":"c","digest":"{config_digest}","size":{}}},
               "layers":[{{"mediaType":"l","digest":"sha256:{}","size":5}}]}}"#,
            config.len(),
            "a".repeat(64)
        );
        let child_digest = Digest::of(child.as_bytes());
        let index = format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_INDEX}","manifests":[
               {{"mediaType":"{OCI_MANIFEST}","digest":"{child_digest}","size":1,"platform":{{"architecture":"amd64","os":"linux"}}}},
               {{"mediaType":"{OCI_MANIFEST}","digest":"sha256:{}","size":1,"platform":{{"architecture":"arm64","os":"linux"}}}}]}}"#,
            "b".repeat(64)
        );
        let index_digest = Digest::of(index.as_bytes());

        let fake = Arc::new(Fake::default());
        let latest = format!("{BASE}/manifests/latest");
        fake.route(&latest, 401, &[("www-authenticate", CHALLENGE)], "");
        fake.route(&latest, 200, &[], index);
        fake.route(TOKEN_URL, 200, &[], r#"{"token":"tok123"}"#);
        fake.route(
            &format!("{BASE}/manifests/{child_digest}"),
            200,
            &[],
            child_body_override.map(<[u8]>::to_vec).unwrap_or_else(|| child.into_bytes()),
        );
        fake.route(&format!("{BASE}/blobs/{config_digest}"), 200, &[], config.to_vec());
        Fixture { fake, index_digest, child_digest, config_digest }
    }

    fn alpine() -> Reference {
        "alpine".parse().unwrap()
    }

    #[test]
    fn resolves_through_auth_and_index() {
        let fx = fixture(None);
        let client = RegistryClient::new(fx.fake.clone());
        let img = client.resolve(&alpine(), &Platform::new("linux", "amd64")).unwrap();

        assert_eq!(img.manifest_digest, fx.child_digest);
        assert_ne!(img.manifest_digest, fx.index_digest);
        assert_eq!(img.config_digest, fx.config_digest);
        assert_eq!(img.manifest.layers.len(), 1);
        assert_eq!(img.config.config.cmd.as_deref(), Some(&["/bin/sh".to_string()][..]));

        // First manifest request anonymous, then every later one uses the cached token.
        let manifest_auth = fx.fake.auth_headers_for("/manifests/latest");
        assert_eq!(manifest_auth, vec![None, Some("Bearer tok123".to_string())]);
        assert_eq!(
            fx.fake.auth_headers_for(&format!("/manifests/{}", fx.child_digest)),
            vec![Some("Bearer tok123".to_string())]
        );
        assert_eq!(
            fx.fake.auth_headers_for("/blobs/"),
            vec![Some("Bearer tok123".to_string())]
        );
    }

    #[test]
    fn sends_accept_header_for_all_manifest_types() {
        let fx = fixture(None);
        RegistryClient::new(fx.fake.clone())
            .resolve(&alpine(), &Platform::new("linux", "amd64"))
            .unwrap();
        let calls = fx.fake.calls.lock().unwrap();
        let accept = &calls[0].1.iter().find(|(k, _)| k == "Accept").unwrap().1;
        for mt in ACCEPTED {
            assert!(accept.contains(mt), "{mt}");
        }
    }

    #[test]
    fn rejects_tampered_platform_manifest() {
        let fx = fixture(Some(br#"{"schemaVersion":2,"config":{},"layers":[]}"#));
        let err = RegistryClient::new(fx.fake.clone())
            .resolve(&alpine(), &Platform::new("linux", "amd64"))
            .unwrap_err();
        assert!(matches!(err, OciError::DigestMismatch { .. }), "{err}");
    }

    #[test]
    fn errors_when_platform_missing() {
        let fx = fixture(None);
        let err = RegistryClient::new(fx.fake.clone())
            .resolve(&alpine(), &Platform::new("linux", "riscv64"))
            .unwrap_err();
        assert!(matches!(err, OciError::NoMatchingPlatform { .. }), "{err}");
    }

    #[test]
    fn plain_docker_manifest_without_index() {
        let config = br#"{"architecture":"amd64","os":"linux"}"#;
        let cd = Digest::of(config);
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{DOCKER_MANIFEST}","config":{{"mediaType":"c","digest":"{cd}","size":1}},"layers":[]}}"#
        );
        let fake = Arc::new(Fake::default());
        fake.route(&format!("{BASE}/manifests/latest"), 200, &[], manifest.clone());
        fake.route(&format!("{BASE}/blobs/{cd}"), 200, &[], config.to_vec());
        let img = RegistryClient::new(fake).resolve(&alpine(), &Platform::host()).unwrap();
        assert_eq!(img.manifest_digest, Digest::of(manifest.as_bytes()));
    }

    #[test]
    fn http_errors_surface_status() {
        let fake = Arc::new(Fake::default());
        fake.route(&format!("{BASE}/manifests/latest"), 404, &[], "");
        let err = RegistryClient::new(fake).resolve(&alpine(), &Platform::host()).unwrap_err();
        assert!(matches!(err, OciError::Http { status: 404, .. }), "{err}");
    }

    #[test]
    fn second_401_after_token_is_an_error() {
        let fake = Arc::new(Fake::default());
        let latest = format!("{BASE}/manifests/latest");
        fake.route(&latest, 401, &[("www-authenticate", CHALLENGE)], "");
        fake.route(TOKEN_URL, 200, &[], r#"{"token":"t"}"#);
        let err = RegistryClient::new(fake).resolve(&alpine(), &Platform::host()).unwrap_err();
        assert!(matches!(err, OciError::Http { status: 401, .. }), "{err}");
    }
}
