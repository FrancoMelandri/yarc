//! Path handling for a rootfs that came from an untrusted image.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Result, RuntimeError};

/// Joins `rel` onto `rootfs`, refusing to go through symlinks or `..`.
///
/// Mount targets are created inside the rootfs *before* `pivot_root`, so a
/// symlink such as `rootfs/proc -> /etc` planted by an image would otherwise
/// redirect our mounts and file writes to the host. Missing components are
/// fine (the caller creates them).
pub fn safe_join(rootfs: &Path, rel: &str) -> Result<PathBuf> {
    join(rootfs, rel, true)
}

/// Like [`safe_join`], but a symlink as the *last* component is allowed: the
/// caller replaces it (used for generated files such as `etc/resolv.conf`).
pub fn safe_join_file(rootfs: &Path, rel: &str) -> Result<PathBuf> {
    join(rootfs, rel, false)
}

fn join(rootfs: &Path, rel: &str, check_last: bool) -> Result<PathBuf> {
    let parts: Vec<&str> = rel.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    let mut cur = rootfs.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        if *part == ".." {
            return Err(RuntimeError::UnsafePath(rel.to_string()));
        }
        cur.push(part);
        let last = i + 1 == parts.len();
        if last && !check_last {
            continue;
        }
        if fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(RuntimeError::UnsafePath(rel.to_string()));
        }
    }
    Ok(cur)
}

/// Finds the program to execute the way `execvp` would, but using the
/// *container's* `PATH` (the host's must not leak in). Names containing a `/`
/// are used as they are.
pub fn resolve_executable(arg0: &str, path_var: &str, is_executable: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    if arg0.contains('/') {
        return Some(PathBuf::from(arg0));
    }
    path_var
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(arg0))
        .find(|candidate| is_executable(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_missing_components_are_joined() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("dev")).unwrap();
        assert_eq!(safe_join(root.path(), "dev/pts").unwrap(), root.path().join("dev/pts"));
        assert_eq!(safe_join(root.path(), "a/./b//c").unwrap(), root.path().join("a/b/c"));
    }

    #[test]
    fn dotdot_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(safe_join(root.path(), "../etc"), Err(RuntimeError::UnsafePath(_))));
        assert!(matches!(safe_join(root.path(), "dev/../../etc"), Err(RuntimeError::UnsafePath(_))));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_rejected_anywhere_in_the_path() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("proc")).unwrap();
        fs::create_dir(root.path().join("dev")).unwrap();
        symlink(outside.path(), root.path().join("dev/pts")).unwrap();

        assert!(matches!(safe_join(root.path(), "proc"), Err(RuntimeError::UnsafePath(_))));
        assert!(matches!(safe_join(root.path(), "proc/sub"), Err(RuntimeError::UnsafePath(_))));
        assert!(matches!(safe_join(root.path(), "dev/pts"), Err(RuntimeError::UnsafePath(_))));
    }

    #[cfg(unix)]
    #[test]
    fn a_final_symlink_is_allowed_for_files_only() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("etc")).unwrap();
        symlink("/run/resolv.conf", root.path().join("etc/resolv.conf")).unwrap();
        assert!(safe_join_file(root.path(), "etc/resolv.conf").is_ok());
        assert!(safe_join(root.path(), "etc/resolv.conf").is_err());
        // but a symlinked directory on the way is still refused
        symlink("/elsewhere", root.path().join("var")).unwrap();
        assert!(safe_join_file(root.path(), "var/x").is_err());
    }

    #[test]
    fn executables_are_searched_in_the_given_path_only() {
        let found = |p: &Path| p == Path::new("/usr/bin/echo") || p == Path::new("/bin/ls");
        assert_eq!(resolve_executable("echo", "/sbin:/usr/bin:/bin", found), Some(PathBuf::from("/usr/bin/echo")));
        assert_eq!(resolve_executable("ls", "/usr/bin:/bin", found), Some(PathBuf::from("/bin/ls")));
        assert_eq!(resolve_executable("nope", "/usr/bin:/bin", found), None);
        assert_eq!(resolve_executable("echo", "", found), None);
        assert_eq!(resolve_executable("echo", "::/usr/bin", found), Some(PathBuf::from("/usr/bin/echo")));
    }

    #[test]
    fn names_with_a_slash_skip_the_search() {
        assert_eq!(resolve_executable("/bin/sh", "/nowhere", |_| false), Some(PathBuf::from("/bin/sh")));
        assert_eq!(resolve_executable("./run.sh", "/nowhere", |_| false), Some(PathBuf::from("./run.sh")));
    }
}
