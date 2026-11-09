//! Windows paths -> the Linux paths of the WSL distro that runs the agent.
//!
//! Works on the text of the path, whatever OS it runs on, so it is testable
//! everywhere. Relative paths must be made absolute by the caller first
//! (`absolutize`).

use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum PathError {
    /// `data\x`, `\x` (relative to the current drive) or `C:x` (to a drive's current directory).
    NotAbsolute(String),
    /// `\\server\share`: WSL cannot see it.
    Network(String),
    /// `\\wsl$\Other\...` while the agent runs in a different distro.
    OtherDistro { path: String, distro: String },
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::NotAbsolute(p) => write!(f, "'{p}' is not an absolute path"),
            PathError::Network(p) => write!(f, "'{p}' is a network path; WSL can only use local drives and its own files"),
            PathError::OtherDistro { path, distro } => write!(f, "'{path}' belongs to another WSL distro than {distro}"),
        }
    }
}

impl std::error::Error for PathError {}

/// `C:\a\b` -> `/mnt/c/a/b`; `\\wsl$\<distro>\home\u` -> `/home/u`; a path that is already
/// a Linux path is returned unchanged. `distro` is the distro the agent runs in, when known.
pub fn to_linux(path: &str, distro: Option<&str>) -> Result<String, PathError> {
    // `\\?\C:\x` is the same path in its "verbatim" spelling
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);

    if path.starts_with('/') && !path.starts_with("//") {
        return Ok(path.to_string());
    }
    if let Some(unc) = path.strip_prefix(r"\\").or_else(|| path.strip_prefix("//")) {
        return unc_to_linux(path, unc, distro);
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let rest = &path[2..];
        if !rest.starts_with(['\\', '/']) && !rest.is_empty() {
            return Err(PathError::NotAbsolute(path.to_string()));
        }
        let drive = (bytes[0] as char).to_ascii_lowercase();
        return Ok(join_under(&format!("/mnt/{drive}"), rest));
    }
    Err(PathError::NotAbsolute(path.to_string()))
}

fn unc_to_linux(whole: &str, unc: &str, distro: Option<&str>) -> Result<String, PathError> {
    let mut parts = unc.splitn(3, ['\\', '/']);
    let host = parts.next().unwrap_or("");
    let share = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("");
    let is_wsl = host.eq_ignore_ascii_case("wsl$") || host.eq_ignore_ascii_case("wsl.localhost");
    if !is_wsl || share.is_empty() {
        return Err(PathError::Network(whole.to_string()));
    }
    if let Some(d) = distro {
        if !share.eq_ignore_ascii_case(d) {
            return Err(PathError::OtherDistro { path: whole.to_string(), distro: d.to_string() });
        }
    }
    Ok(join_under("", rest))
}

/// `base` + the separators-normalised `rest`, with `.` and `..` resolved (never above `base`).
fn join_under(base: &str, rest: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split(['\\', '/']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        return if base.is_empty() { "/".into() } else { base.to_string() };
    }
    format!("{base}/{}", parts.join("/"))
}

/// Makes `path` absolute using the current directory (Windows: also resolves a
/// drive-relative or root-relative form). Linux paths come back as they are.
pub fn absolutize(path: &std::path::Path) -> std::io::Result<String> {
    Ok(std::path::absolute(path)?.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(path: &str) -> String {
        to_linux(path, Some("Ubuntu-24.04")).unwrap()
    }

    #[test]
    fn drive_paths_go_under_mnt() {
        assert_eq!(ok(r"C:\Projects\MINE\yarc"), "/mnt/c/Projects/MINE/yarc");
        assert_eq!(ok("d:/data/x"), "/mnt/d/data/x");
        assert_eq!(ok(r"C:\"), "/mnt/c");
        assert_eq!(ok("C:"), "/mnt/c");
        assert_eq!(ok(r"C:\a\\b\.\c\..\d\"), "/mnt/c/a/b/d");
        assert_eq!(ok(r"C:\..\..\x"), "/mnt/c/x", ".. never leaves the drive");
    }

    #[test]
    fn verbatim_prefix_is_the_same_path() {
        assert_eq!(ok(r"\\?\C:\Users\me\yarc.yaml"), "/mnt/c/Users/me/yarc.yaml");
    }

    #[test]
    fn wsl_shares_map_back_to_linux_paths() {
        assert_eq!(ok(r"\\wsl$\Ubuntu-24.04\home\me\app"), "/home/me/app");
        assert_eq!(ok(r"\\wsl.localhost\ubuntu-24.04\home\me"), "/home/me");
        assert_eq!(ok(r"\\wsl$\Ubuntu-24.04"), "/");
        assert_eq!(to_linux(r"\\wsl$\Any\tmp", None).unwrap(), "/tmp");
        assert!(matches!(to_linux(r"\\wsl$\Debian\tmp", Some("Ubuntu-24.04")), Err(PathError::OtherDistro { .. })));
    }

    #[test]
    fn linux_paths_pass_through() {
        assert_eq!(ok("/home/me/x"), "/home/me/x");
        assert_eq!(ok("/"), "/");
    }

    #[test]
    fn what_cannot_be_translated_is_an_error() {
        for bad in [r"data\x", "yarc.yaml", r"\x", "C:x", "", "./a"] {
            assert!(matches!(to_linux(bad, None), Err(PathError::NotAbsolute(_))), "{bad:?}");
        }
        for net in [r"\\server\share\x", "//server/share", r"\\wsl$"] {
            assert!(matches!(to_linux(net, None), Err(PathError::Network(_))), "{net:?}");
        }
    }

    #[test]
    fn errors_name_the_path() {
        assert!(to_linux(r"\\srv\s", None).unwrap_err().to_string().contains(r"\\srv\s"));
    }
}
