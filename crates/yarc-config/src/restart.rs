//! Restart policies (`restart:` of a service, `--restart` of `run`/`create`). Only the
//! vocabulary lives here; the decisions and the supervisor are in `yarc-runtime`.
//!
//! See `docs/progress/C4-restart-design.md`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// `restart:` of a service, `--restart` of `run`/`create`.
///
/// `always` and `unless-stopped` behave the same: they only differ after a daemon or host
/// restart, and yarc has no boot-time supervisor. Both spellings are kept for compose files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum RestartPolicy {
    #[default]
    No,
    Always,
    UnlessStopped,
    /// Restart after a non-zero exit, at most `max` times (`None`: no limit).
    OnFailure {
        max: Option<u32>,
    },
}

impl RestartPolicy {
    pub fn is_no(&self) -> bool {
        *self == RestartPolicy::No
    }
}

impl fmt::Display for RestartPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestartPolicy::No => f.write_str("no"),
            RestartPolicy::Always => f.write_str("always"),
            RestartPolicy::UnlessStopped => f.write_str("unless-stopped"),
            RestartPolicy::OnFailure { max: None } => f.write_str("on-failure"),
            RestartPolicy::OnFailure { max: Some(n) } => write!(f, "on-failure:{n}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsePolicyError(String);

impl fmt::Display for ParsePolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParsePolicyError {}

impl FromStr for RestartPolicy {
    type Err = ParsePolicyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = |why: &str| ParsePolicyError(format!("invalid restart policy '{s}': {why}"));
        match s {
            "no" => Ok(RestartPolicy::No),
            "always" => Ok(RestartPolicy::Always),
            "unless-stopped" => Ok(RestartPolicy::UnlessStopped),
            "on-failure" => Ok(RestartPolicy::OnFailure { max: None }),
            _ => match s.strip_prefix("on-failure:") {
                Some(n) => match n.parse::<u32>() {
                    Ok(0) => Err(bad("the maximum must be at least 1 (use 'on-failure' for no limit)")),
                    Ok(max) => Ok(RestartPolicy::OnFailure { max: Some(max) }),
                    Err(_) => Err(bad("the maximum must be a positive number")),
                },
                None => Err(bad("expected no, always, unless-stopped, on-failure or on-failure:N")),
            },
        }
    }
}

impl TryFrom<String> for RestartPolicy {
    type Error = ParsePolicyError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<RestartPolicy> for String {
    fn from(p: RestartPolicy) -> String {
        p.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_parse_and_print() {
        for (text, policy) in [
            ("no", RestartPolicy::No),
            ("always", RestartPolicy::Always),
            ("unless-stopped", RestartPolicy::UnlessStopped),
            ("on-failure", RestartPolicy::OnFailure { max: None }),
            ("on-failure:3", RestartPolicy::OnFailure { max: Some(3) }),
        ] {
            assert_eq!(text.parse::<RestartPolicy>().unwrap(), policy, "{text}");
            assert_eq!(policy.to_string(), text);
        }
        assert!(RestartPolicy::default().is_no());
    }

    #[test]
    fn bad_policies_are_rejected_with_a_reason() {
        for (text, needle) in [
            ("", "expected"),
            ("sometimes", "expected"),
            ("Always", "expected"),
            ("always:3", "expected"),
            ("on-failure:", "positive number"),
            ("on-failure:x", "positive number"),
            ("on-failure:-1", "positive number"),
            ("on-failure:0", "at least 1"),
            ("on-failure:3:4", "positive number"),
            ("on-failure:99999999999", "positive number"),
        ] {
            let e = text.parse::<RestartPolicy>().unwrap_err().to_string();
            assert!(e.contains(needle) && e.contains(&format!("'{text}'")), "{text}: {e}");
        }
    }

    #[test]
    fn policies_serialize_as_their_text() {
        let json = serde_json::to_string(&RestartPolicy::OnFailure { max: Some(2) }).unwrap();
        assert_eq!(json, "\"on-failure:2\"");
        assert_eq!(serde_json::from_str::<RestartPolicy>(&json).unwrap(), RestartPolicy::OnFailure { max: Some(2) });
        assert!(serde_json::from_str::<RestartPolicy>("\"nope\"").is_err());
    }
}
