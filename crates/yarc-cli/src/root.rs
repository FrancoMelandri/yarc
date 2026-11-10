//! Default location of the yarc data directory.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{anyhow, Result};

pub fn default_root() -> Result<PathBuf> {
    locate(|k| std::env::var_os(k))
        .ok_or_else(|| anyhow!("cannot determine a data directory; pass --root or set YARC_ROOT"))
}

/// Pure so it can be tested; `env` looks up an environment variable.
fn locate(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let non_empty = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        non_empty("LOCALAPPDATA").map(|p| p.join("yarc"))
    } else if cfg!(target_os = "macos") {
        non_empty("HOME").map(|p| p.join("Library/Application Support/yarc"))
    } else {
        non_empty("XDG_DATA_HOME").or_else(|| non_empty("HOME").map(|h| h.join(".local/share"))).map(|p| p.join("yarc"))
    }
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| OsString::from(v))
    }

    #[test]
    fn prefers_xdg_data_home() {
        let e = env(&[("XDG_DATA_HOME", "/data"), ("HOME", "/home/u")]);
        assert_eq!(locate(e), Some(PathBuf::from("/data/yarc")));
    }

    #[test]
    fn falls_back_to_home() {
        assert_eq!(locate(env(&[("HOME", "/home/u")])), Some(PathBuf::from("/home/u/.local/share/yarc")));
        assert_eq!(
            locate(env(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "")])),
            Some(PathBuf::from("/home/u/.local/share/yarc"))
        );
    }

    #[test]
    fn none_without_home() {
        assert_eq!(locate(env(&[])), None);
    }
}
