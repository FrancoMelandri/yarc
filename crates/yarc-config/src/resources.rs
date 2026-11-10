//! CPU and memory limits of a container (`--memory`, `mem_limit`, `resources:` ...). Only the
//! vocabulary and its validation live here; enforcing them is `yarc-runtime`'s business.
//!
//! See `docs/progress/L-limits-plan.md`.

use std::fmt;

use serde::{Deserialize, Serialize};

const KIB: u64 = 1024;
/// Docker's floor: a smaller limit cannot even start a process.
pub const MIN_MEMORY: u64 = 6 * KIB * KIB;
/// The cgroup interface stores limits as signed 64-bit values.
const MAX_BYTES: u64 = i64::MAX as u64;

/// What a container may use. Every field is optional; the default is "no limits".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Resources {
    /// Memory limit in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<u64>,
    /// Memory **plus** swap in bytes, like Docker's `--memory-swap`. `None` with a `memory`
    /// limit means no swap at all (the container is killed instead of paging out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_swap: Option<u64>,
    /// CPU limit in thousandths of a CPU: 500 is half a CPU, 2000 two CPUs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus_millis: Option<u32>,
    /// Maximum number of processes and threads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u64>,
}

/// A rejected limit: `field` is the canonical name (`memory`, `memory_swap`, `cpus`, `pids`),
/// so each reader can name it the way the user wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceError {
    pub field: &'static str,
    pub reason: String,
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}

impl std::error::Error for ResourceError {}

impl Resources {
    pub fn is_unset(&self) -> bool {
        *self == Resources::default()
    }

    /// Checks the limits against each other (the individual values are checked by their
    /// parsers): swap needs a memory limit and cannot be below it.
    pub fn check(&self) -> Result<(), ResourceError> {
        let err = |reason: &str| ResourceError { field: "memory_swap", reason: reason.into() };
        match (self.memory, self.memory_swap) {
            (None, Some(_)) => Err(err("needs a memory limit as well")),
            (Some(m), Some(s)) if s < m => Err(err("is memory plus swap, so it cannot be below the memory limit")),
            _ => Ok(()),
        }
    }

    /// Swap the container may use on top of its memory: none unless `memory_swap` asks for it.
    pub fn swap_bytes(&self) -> Option<u64> {
        self.memory.map(|m| self.memory_swap.map_or(0, |s| s - m))
    }
}

/// Human form, also used in the definition hash: `memory 512m, swap 256m, cpus 0.5, pids 200`.
impl fmt::Display for Resources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if let Some(m) = self.memory {
            parts.push(format!("memory {}", format_size(m)));
        }
        if let Some(s) = self.swap_bytes().filter(|s| *s > 0) {
            parts.push(format!("swap {}", format_size(s)));
        }
        if let Some(c) = self.cpus_millis {
            parts.push(format!("cpus {}", format_cpus(c)));
        }
        if let Some(p) = self.pids {
            parts.push(format!("pids {p}"));
        }
        f.write_str(&parts.join(", "))
    }
}

fn err(field: &'static str, reason: impl Into<String>) -> ResourceError {
    ResourceError { field, reason: reason.into() }
}

/// `512m`, `1g`, `1.5g`, `256mb`, `64k` or plain bytes; case-insensitive, powers of 1024.
fn parse_bytes(field: &'static str, text: &str) -> Result<u64, ResourceError> {
    let t = text.trim().to_ascii_lowercase();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (number, unit) = t.split_at(split);
    let factor = match unit.trim() {
        "" | "b" => 1,
        "k" | "kb" => KIB,
        "m" | "mb" => KIB * KIB,
        "g" | "gb" => KIB * KIB * KIB,
        "t" | "tb" => KIB * KIB * KIB * KIB,
        _ => return Err(err(field, format!("invalid size '{text}': unit must be b, k, m, g or t"))),
    };
    let invalid = || err(field, format!("invalid size '{text}': expected a number such as 512m or 1.5g"));
    if number.is_empty() || number.starts_with('.') || number.ends_with('.') {
        return Err(invalid());
    }
    let too_big = || err(field, format!("size '{text}' is too large"));
    let bytes = if number.contains('.') {
        if factor == 1 {
            return Err(err(field, format!("invalid size '{text}': bytes cannot have a fraction")));
        }
        let value: f64 = number.parse().map_err(|_| invalid())?;
        let bytes = value * factor as f64;
        if !bytes.is_finite() || bytes >= MAX_BYTES as f64 {
            return Err(too_big());
        }
        bytes as u64
    } else {
        let value: u64 = number.parse().map_err(|_| too_big())?;
        value.checked_mul(factor).filter(|b| *b <= MAX_BYTES).ok_or_else(too_big)?
    };
    Ok(bytes)
}

/// A memory limit (at least [`MIN_MEMORY`]).
pub fn parse_memory(text: &str) -> Result<u64, ResourceError> {
    let bytes = parse_bytes("memory", text)?;
    if bytes < MIN_MEMORY {
        return Err(err("memory", format!("'{text}' is too small, the minimum is 6m")));
    }
    Ok(bytes)
}

/// Memory plus swap: the same sizes as [`parse_memory`].
pub fn parse_memory_swap(text: &str) -> Result<u64, ResourceError> {
    let bytes = parse_bytes("memory_swap", text)?;
    if bytes < MIN_MEMORY {
        return Err(err("memory_swap", format!("'{text}' is too small, the minimum is 6m")));
    }
    Ok(bytes)
}

/// A CPU count such as `0.5`, `1` or `2.25`, in thousandths (the precision kept).
pub fn parse_cpus(text: &str) -> Result<u32, ResourceError> {
    let t = text.trim();
    let invalid = || err("cpus", format!("invalid value '{text}': expected a number such as 0.5 or 2"));
    if t.is_empty() || !t.chars().all(|c| c.is_ascii_digit() || c == '.') || t.starts_with('.') || t.ends_with('.') {
        return Err(invalid());
    }
    let value: f64 = t.parse().map_err(|_| invalid())?;
    let millis = (value * 1000.0).round();
    if millis < 1.0 {
        return Err(err("cpus", format!("'{text}' must be at least 0.001")));
    }
    if millis > u32::MAX as f64 {
        return Err(err("cpus", format!("'{text}' is too large")));
    }
    Ok(millis as u32)
}

/// A process limit: a positive number; `-1` means unlimited (`None`), as in compose.
pub fn parse_pids(text: &str) -> Result<Option<u64>, ResourceError> {
    let t = text.trim();
    if t == "-1" {
        return Ok(None);
    }
    match t.parse::<u64>() {
        Ok(0) | Err(_) => Err(err("pids", format!("invalid value '{text}': expected a positive number or -1"))),
        Ok(n) => Ok(Some(n)),
    }
}

/// The largest unit that divides `bytes` exactly (`536870912` -> `512m`).
pub fn format_size(bytes: u64) -> String {
    for (unit, factor) in [('t', KIB.pow(4)), ('g', KIB.pow(3)), ('m', KIB.pow(2)), ('k', KIB)] {
        if bytes >= factor && bytes.is_multiple_of(factor) {
            return format!("{}{unit}", bytes / factor);
        }
    }
    bytes.to_string()
}

fn format_cpus(millis: u32) -> String {
    let text = format!("{}.{:03}", millis / 1000, millis % 1000);
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_binary_units_in_any_case() {
        let m = |t: &str| parse_memory(t).unwrap();
        assert_eq!(m("512m"), 512 * 1024 * 1024);
        assert_eq!(m("512M"), 512 * 1024 * 1024);
        assert_eq!(m("256mb"), 256 * 1024 * 1024);
        assert_eq!(m("1g"), 1 << 30);
        assert_eq!(m("1.5g"), 3 << 29);
        assert_eq!(m("0.5g"), 1 << 29);
        assert_eq!(m("8192k"), 8 << 20);
        assert_eq!(m("134217728"), 128 << 20);
        assert_eq!(m(" 64m "), 64 << 20);
    }

    #[test]
    fn bad_sizes_are_rejected_with_the_offending_text() {
        for bad in ["", "m", "12x", "-5m", "1.2.3g", ".5g", "5.g", "1.5", "1 5m", "0x10"] {
            let e = parse_memory(bad).unwrap_err();
            assert_eq!(e.field, "memory", "{bad}");
            assert!(e.reason.contains("invalid size") || e.reason.contains("too"), "{bad}: {}", e.reason);
        }
        assert!(parse_memory("5m").unwrap_err().reason.contains("minimum is 6m"));
        assert!(parse_memory("6m").is_ok());
        assert!(parse_memory("0").is_err());
        assert!(parse_memory("99999999999t").unwrap_err().reason.contains("too large"));
        assert!(parse_memory("99999999999999999999").unwrap_err().reason.contains("too large"));
        assert_eq!(parse_memory_swap("1m").unwrap_err().field, "memory_swap");
    }

    #[test]
    fn cpus_are_kept_in_thousandths() {
        assert_eq!(parse_cpus("0.5").unwrap(), 500);
        assert_eq!(parse_cpus("1").unwrap(), 1000);
        assert_eq!(parse_cpus("2.25").unwrap(), 2250);
        assert_eq!(parse_cpus("0.001").unwrap(), 1);
        for bad in ["", "0", "0.0004", "-1", "abc", "1e3", ".5", "1.", "99999999"] {
            assert_eq!(parse_cpus(bad).unwrap_err().field, "cpus", "{bad}");
        }
    }

    #[test]
    fn pids_are_positive_and_minus_one_is_unlimited() {
        assert_eq!(parse_pids("200").unwrap(), Some(200));
        assert_eq!(parse_pids("-1").unwrap(), None);
        for bad in ["0", "-2", "x", "", "1.5"] {
            assert_eq!(parse_pids(bad).unwrap_err().field, "pids", "{bad}");
        }
    }

    #[test]
    fn swap_is_memory_plus_swap_and_needs_a_memory_limit() {
        let mb = |n: u64| n << 20;
        let r = Resources { memory: Some(mb(512)), memory_swap: Some(mb(768)), ..Default::default() };
        assert!(r.check().is_ok());
        assert_eq!(r.swap_bytes(), Some(mb(256)));

        let none = Resources { memory: Some(mb(512)), ..Default::default() };
        assert!(none.check().is_ok());
        assert_eq!(none.swap_bytes(), Some(0), "no swap unless asked for");
        assert_eq!(Resources::default().swap_bytes(), None);

        let equal = Resources { memory: Some(mb(512)), memory_swap: Some(mb(512)), ..Default::default() };
        assert!(equal.check().is_ok());
        assert_eq!(equal.swap_bytes(), Some(0));

        let below = Resources { memory: Some(mb(512)), memory_swap: Some(mb(256)), ..Default::default() };
        assert!(below.check().unwrap_err().reason.contains("cannot be below"));
        let alone = Resources { memory_swap: Some(mb(256)), ..Default::default() };
        assert!(alone.check().unwrap_err().reason.contains("needs a memory limit"));
    }

    #[test]
    fn display_is_stable_and_round_trips_the_units() {
        assert_eq!(Resources::default().to_string(), "");
        let r = Resources {
            memory: Some(512 << 20),
            memory_swap: Some(768 << 20),
            cpus_millis: Some(500),
            pids: Some(200),
        };
        assert_eq!(r.to_string(), "memory 512m, swap 256m, cpus 0.5, pids 200");
        assert_eq!(format_size(3 << 29), "1536m");
        assert_eq!(format_size(1000), "1000");
        assert_eq!(format_cpus(2000), "2");
        assert_eq!(format_cpus(2250), "2.25");
    }

    #[test]
    fn only_the_limits_that_are_set_are_serialized() {
        assert_eq!(serde_json::to_string(&Resources::default()).unwrap(), "{}");
        let r = Resources { cpus_millis: Some(1500), ..Default::default() };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"cpus_millis":1500}"#);
        assert_eq!(serde_json::from_str::<Resources>(&json).unwrap(), r);
        assert!(Resources::default().is_unset() && !r.is_unset());
    }
}
