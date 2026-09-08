//! A PID-file lock preventing two `ben_snipes` processes from trading
//! against the same state directory concurrently. Two instances racing
//! on the same `open-positions.json`/`trades.json` could double-buy,
//! double-sell, or corrupt the trade journal, so this is checked before
//! any other state file is touched.
//!
//! Liveness is checked via the `/proc/<pid>` filesystem, which exists
//! on every target this project ships to (Linux, and Android/Termux -
//! also a Linux kernel - per the release workflow). That keeps this
//! dependency-free rather than pulling in an OS-locking crate for a
//! single check. A lock file whose recorded PID no longer corresponds
//! to a running process is stale - the previous process crashed,
//! was killed, or the machine lost power - and is silently reclaimed
//! rather than permanently blocking every future startup.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Held for the lifetime of the process. Dropping it deletes the lock
/// file so the next start doesn't have to wait for stale-PID detection.
/// A path that skips `Drop` - `std::process::exit` called while the
/// lock is still held, or a hard kill - leaves the file behind naming
/// a now-dead PID, which the next `acquire` reclaims automatically via
/// the same staleness check used for a crash. No caller bookkeeping
/// required either way.
pub struct InstanceLock {
    path: PathBuf,
}

impl InstanceLock {
    /// Acquires the lock at `path`, refusing to start if another live
    /// process already holds it.
    pub fn acquire(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create directory for instance lock: {e}"))?;
            }
        }

        if let Some(existing_pid) = Self::read_pid(&path)? {
            if Self::process_is_alive(existing_pid) {
                return Err(format!(
                    "another ben_snipes instance appears to be running (pid {existing_pid}, lock file: {})",
                    path.display()
                ));
            }
            // The recorded process is gone: a crash, `kill -9`, or a
            // power loss left this behind. Fall through and reclaim it.
        }

        let pid = std::process::id();
        let tmp_path = path.with_extension("lock.tmp");
        {
            let mut file = std::fs::File::create(&tmp_path)
                .map_err(|e| format!("failed to write instance lock: {e}"))?;
            write!(file, "{pid}").map_err(|e| format!("failed to write instance lock: {e}"))?;
        }
        std::fs::rename(&tmp_path, &path).map_err(|e| format!("failed to write instance lock: {e}"))?;

        Ok(Self { path })
    }

    /// `Ok(None)` covers both "no lock file yet" and "the lock file
    /// exists but its contents aren't a readable PID" - either way
    /// there's no evidence of a live owner, and refusing to start over
    /// an unreadable-but-harmless file would be worse than reclaiming it.
    fn read_pid(path: &Path) -> Result<Option<u32>, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => Ok(contents.trim().parse::<u32>().ok()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("failed to read instance lock: {e}")),
        }
    }

    #[cfg(target_os = "linux")]
    fn process_is_alive(pid: u32) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
    }

    /// Off Linux there's no dependency-free way to check this. Fail
    /// closed - assume the process might still be alive - rather than
    /// risk two instances trading concurrently on a platform this
    /// project doesn't target anyway.
    #[cfg(not(target_os = "linux"))]
    fn process_is_alive(_pid: u32) -> bool {
        true
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should never be before the epoch in CI")
            .as_nanos();
        std::env::temp_dir().join(format!("ben_snipes-instance-test-{nanos}.lock"))
    }

    #[test]
    fn acquires_a_fresh_lock_and_removes_it_on_drop() {
        let path = temp_path();
        let lock = InstanceLock::acquire(&path).expect("fresh lock should acquire");
        assert!(path.exists());
        assert_eq!(
            std::fs::read_to_string(&path).expect("lock file should be readable").trim(),
            std::process::id().to_string()
        );

        drop(lock);
        assert!(!path.exists(), "dropping the lock should remove the file");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn refuses_to_acquire_while_the_recorded_process_is_alive() {
        let path = temp_path();
        std::fs::write(&path, std::process::id().to_string()).expect("test setup should succeed");

        let result = InstanceLock::acquire(&path);
        assert!(result.is_err(), "a lock file naming this live process should block a second acquire");

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reclaims_a_lock_left_by_a_dead_process() {
        let path = temp_path();
        // Far outside any realistic process table (Linux's pid_max
        // tops out well below this even at its highest configurable
        // setting), so /proc/<pid> is guaranteed not to exist without
        // depending on which specific PIDs happen to be free right now.
        std::fs::write(&path, "999999999").expect("test setup should succeed");

        let lock = InstanceLock::acquire(&path).expect("a lock naming a dead process should be reclaimed");
        assert_eq!(
            std::fs::read_to_string(&path).expect("lock file should be readable").trim(),
            std::process::id().to_string()
        );
        drop(lock);
    }

    #[test]
    fn a_corrupted_lock_file_does_not_permanently_block_startup() {
        let path = temp_path();
        std::fs::write(&path, "not-a-pid").expect("test setup should succeed");

        let lock = InstanceLock::acquire(&path);
        assert!(lock.is_ok(), "an unreadable lock file should not permanently block startup");
        drop(lock);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_lock_file_acquires_cleanly() {
        let path = temp_path();
        let lock = InstanceLock::acquire(&path);
        assert!(lock.is_ok());
    }
}
