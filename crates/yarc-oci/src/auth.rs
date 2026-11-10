//! Registry bearer-token authentication (pure parsing/formatting, no I/O).

use serde::Deserialize;

use crate::error::{OciError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BearerChallenge {
    pub realm: String,
    pub service: Option<String>,
    pub scope: Option<String>,
}

/// Parse a `WWW-Authenticate: Bearer realm="...",service="...",scope="..."` header.
pub fn parse_challenge(header: &str) -> Result<BearerChallenge> {
    let header = header.trim();
    let (scheme, params) = header.split_once(char::is_whitespace).unwrap_or((header, ""));
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(OciError::Auth(format!("unsupported auth scheme '{scheme}'")));
    }

    let mut realm = None;
    let mut service = None;
    let mut scope = None;
    for (k, v) in parse_params(params) {
        match k.to_ascii_lowercase().as_str() {
            "realm" => realm = Some(v),
            "service" => service = Some(v),
            "scope" => scope = Some(v),
            _ => {}
        }
    }
    let realm = realm.ok_or_else(|| OciError::Auth("challenge has no realm".into()))?;
    Ok(BearerChallenge { realm, service, scope })
}

/// Parse `k="v",k2=v2` honouring quotes (quoted values may contain commas).
fn parse_params(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ',') {
            chars.next();
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' {
                break;
            }
            key.push(c);
            chars.next();
        }
        if chars.next().is_none() {
            break; // no '=' found
        }
        let mut value = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            value.push(n);
                        }
                    }
                    '"' => break,
                    _ => value.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                value.push(c);
                chars.next();
            }
        }
        out.push((key.trim().to_string(), value.trim().to_string()));
    }
    out
}

/// Build the token endpoint URL. `default_scope` is used when the server sent none.
pub fn token_url(ch: &BearerChallenge, default_scope: &str) -> String {
    let mut url = ch.realm.clone();
    let mut sep = if url.contains('?') { '&' } else { '?' };
    if let Some(service) = &ch.service {
        url.push_str(&format!("{sep}service={}", percent_encode(service)));
        sep = '&';
    }
    let scope = ch.scope.as_deref().unwrap_or(default_scope);
    url.push_str(&format!("{sep}scope={}", percent_encode(scope)));
    url
}

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

pub fn parse_token_response(body: &[u8]) -> Result<String> {
    let r: TokenResponse =
        serde_json::from_slice(body).map_err(|e| OciError::Auth(format!("invalid token response: {e}")))?;
    r.token
        .or(r.access_token)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| OciError::Auth("token response has no token".into()))
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_hub_challenge() {
        let h = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#;
        let c = parse_challenge(h).unwrap();
        assert_eq!(c.realm, "https://auth.docker.io/token");
        assert_eq!(c.service.as_deref(), Some("registry.docker.io"));
        assert_eq!(c.scope.as_deref(), Some("repository:library/alpine:pull"));
    }

    #[test]
    fn quoted_commas_and_unquoted_values() {
        let c = parse_challenge(r#"bearer realm=https://x/token, scope="repository:a/b:pull,push""#).unwrap();
        assert_eq!(c.realm, "https://x/token");
        assert_eq!(c.scope.as_deref(), Some("repository:a/b:pull,push"));
        assert_eq!(c.service, None);
    }

    #[test]
    fn rejects_basic_and_missing_realm() {
        assert!(matches!(parse_challenge(r#"Basic realm="x""#), Err(OciError::Auth(_))));
        assert!(matches!(parse_challenge(r#"Bearer service="x""#), Err(OciError::Auth(_))));
    }

    #[test]
    fn builds_token_url() {
        let c = BearerChallenge {
            realm: "https://auth.docker.io/token".into(),
            service: Some("registry.docker.io".into()),
            scope: None,
        };
        assert_eq!(
            token_url(&c, "repository:library/alpine:pull"),
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository%3Alibrary%2Falpine%3Apull"
        );
    }

    #[test]
    fn token_response_variants() {
        assert_eq!(parse_token_response(br#"{"token":"a"}"#).unwrap(), "a");
        assert_eq!(parse_token_response(br#"{"access_token":"b"}"#).unwrap(), "b");
        assert!(parse_token_response(br#"{}"#).is_err());
        assert!(parse_token_response(b"nope").is_err());
    }
}
