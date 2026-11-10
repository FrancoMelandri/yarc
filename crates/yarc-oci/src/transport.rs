//! HTTP transport abstraction. The registry client only knows this trait, so
//! the blocking `ureq` implementation can later be joined by an async one.

use std::io::Read;

use crate::error::{OciError, Result};

pub struct Response {
    pub status: u16,
    /// Header names are lowercase.
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read + Send>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Read the whole body, failing if it is larger than `max` bytes.
    pub fn read_limited(mut self, max: u64, what: &str) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        (&mut self.body).take(max + 1).read_to_end(&mut buf).map_err(|e| OciError::Transport(e.to_string()))?;
        if buf.len() as u64 > max {
            return Err(OciError::TooLarge(what.into(), max));
        }
        Ok(buf)
    }
}

pub trait Transport {
    /// Perform a GET. Non-2xx statuses are returned as responses, not errors.
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response>;
}

impl<T: Transport + ?Sized> Transport for std::sync::Arc<T> {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response> {
        (**self).get(url, headers)
    }
}

/// Blocking transport backed by `ureq` (follows redirects, rustls TLS).
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder().http_status_as_error(false).build();
        UreqTransport { agent: ureq::Agent::new_with_config(config) }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for UreqTransport {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response> {
        let mut req = self.agent.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.call().map_err(|e| OciError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = Box::new(resp.into_body().into_reader());
        Ok(Response { status, headers, body })
    }
}
