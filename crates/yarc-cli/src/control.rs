//! `stop`, `kill`, `wait` and `rm` for containers that may be running.

#[cfg(target_os = "linux")]
mod imp {
    use std::path::Path;
    use std::time::Duration;

    use anyhow::{anyhow, Result};
    use yarc_runtime::ContainerStore;

    use crate::cli::{KillArgs, RmArgs, StopArgs, WaitArgs};

    pub fn stop(root: &Path, args: StopArgs) -> Result<()> {
        let containers = ContainerStore::open(root)?;
        for id in &args.ids {
            yarc_runtime::stop_container(&containers, id, Duration::from_secs(args.time))?;
            println!("{id}");
        }
        Ok(())
    }

    pub fn kill(root: &Path, args: KillArgs) -> Result<()> {
        let signal = parse_signal(&args.signal)?; // before touching any container
        let containers = ContainerStore::open(root)?;
        for id in &args.ids {
            yarc_runtime::signal_container(&containers, id, signal)?;
            println!("{id}");
        }
        Ok(())
    }

    /// Blocks until each container is no longer supervised and prints its exit code.
    pub fn wait(root: &Path, args: WaitArgs) -> Result<()> {
        let containers = ContainerStore::open(root)?;
        for id in &args.ids {
            containers.load(id)?;
            if containers.load_state(id)?.is_none() {
                return Err(anyhow!("container {id} has not been started"));
            }
            containers.wait_exit(id, None)?;
            match containers.load_state(id)? {
                Some(state) if state.exit_code.is_some() => println!("{}", state.exit_code.unwrap_or_default()),
                _ => return Err(anyhow!("container {id} died without recording an exit status")),
            }
        }
        Ok(())
    }

    pub fn rm(root: &Path, args: RmArgs) -> Result<()> {
        let containers = ContainerStore::open(root)?;
        for id in &args.ids {
            yarc_runtime::remove_container(&containers, id, args.force)?;
            println!("{id}");
        }
        Ok(())
    }

    /// `KILL`, `sigkill`, `SIGUSR1`, `9`, ... → signal number.
    pub(crate) fn parse_signal(s: &str) -> Result<i32> {
        const NAMES: [(&str, i32); 14] = [
            ("HUP", 1),
            ("INT", 2),
            ("QUIT", 3),
            ("ABRT", 6),
            ("KILL", 9),
            ("USR1", 10),
            ("SEGV", 11),
            ("USR2", 12),
            ("PIPE", 13),
            ("ALRM", 14),
            ("TERM", 15),
            ("CONT", 18),
            ("STOP", 19),
            ("WINCH", 28),
        ];
        if let Ok(n) = s.parse::<i32>() {
            return if (1..=31).contains(&n) { Ok(n) } else { Err(anyhow!("invalid signal number '{s}'")) };
        }
        let upper = s.to_ascii_uppercase();
        let name = upper.strip_prefix("SIG").unwrap_or(&upper);
        NAMES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, num)| *num)
            .ok_or_else(|| anyhow!("unknown signal '{s}'"))
    }

    #[cfg(test)]
    mod tests {
        use super::parse_signal;

        #[test]
        fn names_numbers_and_prefixes() {
            assert_eq!(parse_signal("KILL").unwrap(), 9);
            assert_eq!(parse_signal("kill").unwrap(), 9);
            assert_eq!(parse_signal("SIGTERM").unwrap(), 15);
            assert_eq!(parse_signal("sigusr1").unwrap(), 10);
            assert_eq!(parse_signal("2").unwrap(), 2);
            assert_eq!(parse_signal("31").unwrap(), 31);
        }

        #[test]
        fn rejects_unknown_signals() {
            for bad in ["", "SIG", "FOO", "0", "-1", "32", "SIGSIGTERM", "9x"] {
                assert!(parse_signal(bad).is_err(), "{bad:?}");
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::path::Path;

    use anyhow::Result;

    use crate::cli::{KillArgs, RmArgs, StopArgs, WaitArgs};

    const MSG: &str = "managing containers needs Linux; run yarc inside WSL";

    pub fn stop(_root: &Path, _args: StopArgs) -> Result<()> {
        anyhow::bail!(MSG)
    }
    pub fn kill(_root: &Path, _args: KillArgs) -> Result<()> {
        anyhow::bail!(MSG)
    }
    pub fn wait(_root: &Path, _args: WaitArgs) -> Result<()> {
        anyhow::bail!(MSG)
    }
    pub fn rm(_root: &Path, _args: RmArgs) -> Result<()> {
        anyhow::bail!(MSG)
    }
}

pub use imp::{kill, rm, stop, wait};
