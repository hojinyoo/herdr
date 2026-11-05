//! Refuses to start a server against runtime state another host is using.
//!
//! This lock is advisory content, not a kernel lock. `flock` and `fcntl` locks
//! are unreliable over NFS, which is exactly where two hosts can meet in one
//! state dir, so the server reads the holder's identity and compares it.
//! Two hosts starting in the same instant can both pass the check; the lock
//! catches the common case of a second host starting while the first runs.
//! On one host the socket bind is the authority, so a lock naming this host is
//! always replaced once the bind has succeeded.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// Names the lock holder that may be replaced, for when that host is gone.
pub(crate) const TAKE_OVER_ENV_VAR: &str = "HERDR_TAKE_OVER_FROM";
const LOCK_FILE: &str = "herdr.lock";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LockHolder {
    host: String,
    /// The node marker's id, when this host has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    boot_id: Option<String>,
    pid: u32,
    started_at: u64,
}

impl LockHolder {
    fn current() -> Self {
        Self {
            host: crate::platform::hostname().unwrap_or_default(),
            node: crate::config::node_id().map(str::to_string),
            boot_id: crate::platform::boot_id(),
            pid: std::process::id(),
            started_at: unix_now(),
        }
    }

    /// The name an operator passes to take this lock over.
    fn name(&self) -> &str {
        self.node.as_deref().unwrap_or(&self.host)
    }

    /// Node markers decide when both sides have one. Otherwise compare
    /// hostnames, which pods sharing a home may also share, so a differing
    /// boot id marks a different kernel. A reboot changes it too, so a lock a
    /// crash left before a reboot needs the takeover.
    fn same_machine(&self, other: &Self) -> bool {
        if let (Some(left), Some(right)) = (&self.node, &other.node) {
            return left == right;
        }
        if let (Some(left), Some(right)) = (&self.boot_id, &other.boot_id) {
            if left != right {
                return false;
            }
        }
        same_hostname(&self.host, &other.host)
    }
}

/// Removes the lock on drop while it still names this process.
pub(crate) struct HostLockGuard {
    path: PathBuf,
    holder: LockHolder,
}

impl Drop for HostLockGuard {
    fn drop(&mut self) {
        if read_holder(&self.path).ok().flatten().as_ref() == Some(&self.holder) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Fails when another host holds the lock in `dir`. Run before binding a
/// socket: a socket another host bound refuses connections here, so the bind
/// would remove it as stale.
pub(crate) fn check(dir: &Path) -> io::Result<()> {
    let take_over = std::env::var(TAKE_OVER_ENV_VAR).ok();
    check_as(dir, &LockHolder::current(), take_over.as_deref())
}

/// Records this process as the holder. Run after the socket bind succeeds.
pub(crate) fn acquire(dir: &Path) -> io::Result<HostLockGuard> {
    write_holder(dir, LockHolder::current())
}

fn check_as(dir: &Path, me: &LockHolder, take_over: Option<&str>) -> io::Result<()> {
    let path = dir.join(LOCK_FILE);
    let holder = match read_holder(&path) {
        Ok(Some(holder)) => holder,
        Ok(None) => return Ok(()),
        Err(err) => {
            return Err(io::Error::new(
                err.kind(),
                format!(
                    "cannot read the herdr runtime lock at {}: {err}. Herdr cannot tell whether another host is using this state; fix or remove the file, then retry",
                    path.display()
                ),
            ))
        }
    };
    if holder.same_machine(me) {
        return Ok(());
    }
    if take_over == Some(holder.name()) {
        warn!(
            lock = %path.display(),
            holder = %holder.name(),
            pid = holder.pid,
            "taking over runtime state from another host"
        );
        return Ok(());
    }
    let message = held_elsewhere_message(&path, &holder);
    tracing::error!("{message}");
    Err(io::Error::new(io::ErrorKind::AddrInUse, message))
}

fn held_elsewhere_message(path: &Path, holder: &LockHolder) -> String {
    let dir = path.parent().unwrap_or(path);
    let age = unix_now().saturating_sub(holder.started_at);
    format!(
        "herdr runtime state in {dir} is held by {name} (host {host}, pid {pid}, started {age}s ago).\n\
         Two hosts writing one state dir overwrite each other's sockets and sessions.\n\
         Give each host its own state dir: set XDG_STATE_HOME, or write a distinct name to the node marker.\n\
         If {name} is gone for good, take over with:\n  \
         {env}={name} herdr\n\
         lock: {lock}",
        dir = dir.display(),
        name = holder.name(),
        host = holder.host,
        pid = holder.pid,
        env = TAKE_OVER_ENV_VAR,
        lock = path.display(),
    )
}

/// Compares short hostnames: macOS reports `name.local` or a DHCP domain
/// depending on the network.
fn same_hostname(left: &str, right: &str) -> bool {
    let short = |name: &str| {
        name.split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    short(left) == short(right)
}

/// `Ok(None)` for a missing lock. Content that does not parse, for example a
/// truncated file, names no holder and is replaced.
fn read_holder(path: &Path) -> io::Result<Option<LockHolder>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    match serde_json::from_slice(&bytes) {
        Ok(holder) => Ok(Some(holder)),
        Err(err) => {
            warn!(lock = %path.display(), %err, "ignoring unreadable runtime lock content");
            Ok(None)
        }
    }
}

fn write_holder(dir: &Path, holder: LockHolder) -> io::Result<HostLockGuard> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(LOCK_FILE);
    let encoded = serde_json::to_vec(&holder).map_err(io::Error::other)?;
    // Rename keeps readers on other hosts from seeing a half-written lock.
    let temp = dir.join(format!(
        "{LOCK_FILE}.{}.{}.tmp",
        crate::config::node_label(),
        holder.pid
    ));
    std::fs::write(&temp, encoded)?;
    if let Err(err) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    Ok(HostLockGuard { path, holder })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("herdr-host-lock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn holder(host: &str, node: Option<&str>, boot_id: Option<&str>, pid: u32) -> LockHolder {
        LockHolder {
            host: host.to_string(),
            node: node.map(str::to_string),
            boot_id: boot_id.map(str::to_string),
            pid,
            started_at: unix_now(),
        }
    }

    fn write_lock(dir: &Path, holder: &LockHolder) {
        std::fs::write(dir.join(LOCK_FILE), serde_json::to_vec(holder).unwrap()).unwrap();
    }

    #[test]
    fn missing_lock_is_free() {
        let dir = test_dir("missing");
        assert!(check_as(&dir, &holder("alpha", None, None, 1), None).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn same_host_lock_passes_even_with_a_different_domain() {
        let dir = test_dir("same-host");
        write_lock(&dir, &holder("alpha.local", None, Some("boot-a"), 7));
        assert!(check_as(&dir, &holder("alpha", None, Some("boot-a"), 8), None).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn same_host_stale_lock_with_a_dead_pid_is_replaced() {
        let dir = test_dir("stale-pid");
        write_lock(&dir, &holder("alpha", None, None, u32::MAX));
        let me = holder("alpha", None, None, std::process::id());
        assert!(check_as(&dir, &me, None).is_ok());

        let guard = write_holder(&dir, me).unwrap();
        let written = read_holder(&dir.join(LOCK_FILE)).unwrap().unwrap();
        assert_eq!(written.pid, std::process::id());

        drop(guard);
        assert!(!dir.join(LOCK_FILE).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn other_host_lock_refuses_with_holder_path_and_override() {
        let dir = test_dir("other-host");
        write_lock(&dir, &holder("beta", None, None, 4242));

        let err = check_as(&dir, &holder("alpha", None, None, 1), None).unwrap_err();

        let message = err.to_string();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert!(
            message.contains("held by beta (host beta, pid 4242"),
            "{message}"
        );
        assert!(
            message.contains(&dir.join(LOCK_FILE).display().to_string()),
            "{message}"
        );
        assert!(
            message.contains("HERDR_TAKE_OVER_FROM=beta herdr"),
            "{message}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn shared_hostname_with_another_boot_id_is_another_host() {
        let dir = test_dir("boot-id");
        write_lock(&dir, &holder("jupyter-me", None, Some("boot-a"), 10));
        assert!(check_as(&dir, &holder("jupyter-me", None, Some("boot-b"), 11), None).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn node_markers_decide_over_hostnames() {
        let dir = test_dir("node-marker");
        write_lock(
            &dir,
            &holder("jupyter-me", Some("ldev"), Some("boot-a"), 10),
        );

        let err = check_as(
            &dir,
            &holder("jupyter-me", Some("serine"), Some("boot-a"), 11),
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("HERDR_TAKE_OVER_FROM=ldev herdr"),
            "{err}"
        );
        assert!(check_as(
            &dir,
            &holder("other", Some("ldev"), Some("boot-z"), 12),
            None
        )
        .is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn take_over_must_name_the_holder() {
        let dir = test_dir("take-over");
        write_lock(&dir, &holder("beta", None, None, 4242));
        let me = holder("alpha", None, None, 1);

        assert!(check_as(&dir, &me, Some("gamma")).is_err());
        assert!(check_as(&dir, &me, Some("beta")).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn truncated_lock_names_no_holder() {
        let dir = test_dir("truncated");
        let me = holder("alpha", None, None, 1);
        std::fs::write(dir.join(LOCK_FILE), b"{\"host\":\"be").unwrap();
        assert!(check_as(&dir, &me, None).is_ok());
        std::fs::write(dir.join(LOCK_FILE), b"").unwrap();
        assert!(check_as(&dir, &me, None).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unreadable_lock_refuses() {
        let dir = test_dir("unreadable");
        std::fs::create_dir_all(dir.join(LOCK_FILE)).unwrap();

        let err = check_as(&dir, &holder("alpha", None, None, 1), None).unwrap_err();

        assert!(
            err.to_string()
                .contains("cannot read the herdr runtime lock"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn guard_leaves_a_lock_another_process_rewrote() {
        let dir = test_dir("rewritten");
        let guard = write_holder(&dir, holder("alpha", None, None, 1)).unwrap();
        write_lock(&dir, &holder("alpha", None, None, 2));

        drop(guard);

        assert_eq!(read_holder(&dir.join(LOCK_FILE)).unwrap().unwrap().pid, 2);
        let _ = std::fs::remove_dir_all(dir);
    }
}
