//! Test-only scripted transport shared by client and pull tests.

use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::sync::Mutex;

use crate::error::Result;
use crate::transport::{Response, Transport};

type Canned = (u16, Vec<(String, String)>, Vec<u8>);
/// One recorded request: the URL and the headers sent.
type Call = (String, Vec<(String, String)>);

/// Scripted transport: per-URL response queue (the last response repeats).
#[derive(Default)]
pub(crate) struct Fake {
    routes: Mutex<HashMap<String, VecDeque<Canned>>>,
    pub(crate) calls: Mutex<Vec<Call>>,
}

impl Fake {
    pub(crate) fn route(&self, url: &str, status: u16, headers: &[(&str, &str)], body: impl Into<Vec<u8>>) {
        let h = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.routes.lock().unwrap().entry(url.into()).or_default().push_back((status, h, body.into()));
    }

    /// Drop any scripted responses for `url` and script a single new one.
    pub(crate) fn replace(&self, url: &str, status: u16, body: impl Into<Vec<u8>>) {
        self.routes.lock().unwrap().remove(url);
        self.route(url, status, &[], body);
    }

    pub(crate) fn auth_headers_for(&self, url_part: &str) -> Vec<Option<String>> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(u, _)| u.contains(url_part))
            .map(|(_, h)| h.iter().find(|(k, _)| k == "Authorization").map(|(_, v)| v.clone()))
            .collect()
    }

    pub(crate) fn count_calls(&self, url_part: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|(u, _)| u.contains(url_part)).count()
    }
}

impl Transport for Fake {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response> {
        self.calls
            .lock()
            .unwrap()
            .push((url.into(), headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()));
        let mut routes = self.routes.lock().unwrap();
        let q = routes.get_mut(url).unwrap_or_else(|| panic!("unexpected request: {url}"));
        let (status, headers, body) = if q.len() > 1 { q.pop_front().unwrap() } else { q.front().unwrap().clone() };
        Ok(Response { status, headers, body: Box::new(Cursor::new(body)) })
    }
}
