//! Validation of `Request::LockNotify` socket reports.
//!
//! The control socket is reachable by any process running as the same user,
//! and the daemon *connects* to every socket path a `LockNotify` names (to
//! drive an out-of-process lock renderer's mpv). Left unchecked, that is a
//! confused-deputy primitive: a hostile process could make the daemon open an
//! arbitrary `AF_UNIX` path, flood it with mpv IPC commands, or exhaust
//! threads with an enormous list. So every report is filtered here first,
//! and entries that fail are logged and dropped — never connected to.
//!
//! The rules match what Fresco's own reporters actually produce (the X11
//! saver's `lock-saver-<pid>.sock`, the wlroots host's `lock-<connector>.sock`,
//! both directly under [`crate::ipc::socket_dir`]):
//!
//! * at most [`MAX_SOCKETS`] entries (more than that are ignored, not an error);
//! * `path` absolute, NUL-free, at most [`MAX_PATH_BYTES`] (`sun_path` is 108
//!   bytes including the terminating NUL);
//! * `path` located *directly* under the socket dir — no `..`, no
//!   subdirectories — and that directory must not be a symlink (lstat), so a
//!   swapped-in link cannot redirect the connection elsewhere;
//! * `connector` at most [`MAX_CONNECTOR_CHARS`] printable characters.

use std::path::{Component, Path};

use crate::ipc::LockSocket;

/// Most sockets one report may carry — one per output, with headroom.
pub const MAX_SOCKETS: usize = 16;
/// `sockaddr_un.sun_path` is 108 bytes including the NUL.
pub const MAX_PATH_BYTES: usize = 107;
/// Longest connector name accepted (real ones are like `HDMI-A-1`).
pub const MAX_CONNECTOR_CHARS: usize = 64;

/// Why one entry was rejected — for the log line only.
fn check_socket(
    s: &LockSocket,
    socket_dir: &Path,
    is_symlink: &dyn Fn(&Path) -> bool,
) -> Result<(), &'static str> {
    if s.connector.chars().count() > MAX_CONNECTOR_CHARS {
        return Err("connector too long");
    }
    if s.connector.chars().any(char::is_control) {
        return Err("connector has non-printable characters");
    }
    if s.path.contains('\0') {
        return Err("path contains NUL");
    }
    if s.path.len() > MAX_PATH_BYTES {
        return Err("path too long for a unix socket");
    }
    let path = Path::new(&s.path);
    if !path.is_absolute() {
        return Err("path is not absolute");
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("path contains ..");
    }
    if path.file_name().is_none() || path.parent() != Some(socket_dir) {
        return Err("path is not directly under the Fresco socket directory");
    }
    if is_symlink(socket_dir) {
        return Err("socket directory is a symlink or unreadable");
    }
    Ok(())
}

/// Keep only the valid entries of `sockets` (first [`MAX_SOCKETS`] considered),
/// logging each rejection. Pure apart from the injected `is_symlink`, so the
/// rules are unit-testable without a filesystem.
pub fn validate_sockets(
    sockets: Vec<LockSocket>,
    socket_dir: &Path,
    is_symlink: &dyn Fn(&Path) -> bool,
) -> Vec<LockSocket> {
    if sockets.len() > MAX_SOCKETS {
        log::warn!(
            "lock: LockNotify carried {} sockets; ignoring all but the first {MAX_SOCKETS}",
            sockets.len()
        );
    }
    sockets
        .into_iter()
        .take(MAX_SOCKETS)
        .filter(|s| match check_socket(s, socket_dir, is_symlink) {
            Ok(()) => true,
            Err(why) => {
                log::warn!(
                    "lock: ignoring LockNotify socket {:?} ({:?}): {why}",
                    s.path,
                    s.connector
                );
                false
            }
        })
        .collect()
}

/// `lstat` the directory: a symlink — or anything we cannot stat — fails
/// closed.
pub fn dir_is_symlink_or_unreadable(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir).map_or(true, |m| m.file_type().is_symlink())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const DIR: &str = "/run/user/1000/fresco";

    fn sock(connector: &str, path: &str) -> LockSocket {
        LockSocket {
            connector: connector.to_string(),
            path: path.to_string(),
        }
    }

    fn run(sockets: Vec<LockSocket>) -> Vec<LockSocket> {
        validate_sockets(sockets, Path::new(DIR), &|_| false)
    }

    #[test]
    fn accepts_what_fresco_itself_reports() {
        let ok = vec![
            sock("DP-1", "/run/user/1000/fresco/lock-DP-1.sock"),
            sock("", "/run/user/1000/fresco/lock-saver-4242.sock"),
        ];
        assert_eq!(run(ok.clone()), ok);
    }

    #[test]
    fn rejects_paths_outside_or_below_the_socket_dir() {
        for p in [
            "/tmp/evil.sock",
            "/run/user/1000/fresco/sub/x.sock",
            "/run/user/1000/other/x.sock",
            "/run/user/1000/fresco/../x.sock",
            "/run/user/1000/fresco/../fresco/x.sock",
            "relative.sock",
            "/run/user/1000/fresco/",
            "/run/user/1000/fresco",
        ] {
            assert!(
                run(vec![sock("DP-1", p)]).is_empty(),
                "{p} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_nul_and_overlong_paths() {
        assert!(run(vec![sock("DP-1", "/run/user/1000/fresco/a\0b.sock")]).is_empty());
        let long = format!("{DIR}/{}", "a".repeat(MAX_PATH_BYTES));
        assert!(run(vec![sock("DP-1", &long)]).is_empty());
        // Exactly 107 bytes is fine.
        let fit = format!("{DIR}/{}", "a".repeat(MAX_PATH_BYTES - DIR.len() - 1));
        assert_eq!(fit.len(), MAX_PATH_BYTES);
        assert_eq!(run(vec![sock("DP-1", &fit)]).len(), 1);
    }

    #[test]
    fn rejects_bad_connectors() {
        let p = "/run/user/1000/fresco/lock-x.sock";
        assert!(run(vec![sock(&"a".repeat(65), p)]).is_empty());
        assert_eq!(run(vec![sock(&"a".repeat(64), p)]).len(), 1);
        assert!(run(vec![sock("DP\n1", p)]).is_empty());
        assert!(run(vec![sock("DP\u{7}1", p)]).is_empty());
    }

    #[test]
    fn caps_the_count_at_sixteen() {
        let many: Vec<_> = (0..40)
            .map(|i| sock("DP-1", &format!("{DIR}/lock-{i}.sock")))
            .collect();
        let kept = run(many);
        assert_eq!(kept.len(), MAX_SOCKETS);
        assert!(kept[15].path.ends_with("lock-15.sock"));
    }

    #[test]
    fn invalid_entries_are_dropped_but_valid_neighbours_survive() {
        let good = sock("DP-1", "/run/user/1000/fresco/lock-DP-1.sock");
        let kept = run(vec![sock("X", "/etc/passwd"), good.clone()]);
        assert_eq!(kept, vec![good]);
    }

    #[test]
    fn a_symlinked_socket_dir_rejects_everything() {
        let good = sock("DP-1", "/run/user/1000/fresco/lock-DP-1.sock");
        let kept = validate_sockets(vec![good], Path::new(DIR), &|_| true);
        assert!(kept.is_empty());
    }

    #[test]
    fn lstat_helper_detects_symlinks_and_missing_dirs() {
        let base: PathBuf = std::env::temp_dir().join(format!(
            "fresco-notify-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let real = base.join("real");
        let link = base.join("link");
        std::fs::create_dir_all(&real).unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(!dir_is_symlink_or_unreadable(&real));
        assert!(dir_is_symlink_or_unreadable(&link));
        assert!(dir_is_symlink_or_unreadable(&base.join("missing")));
        let _ = std::fs::remove_dir_all(&base);
    }
}
